//! Filling a cache daemon's store from the server (SPEC D8, §3 "Object
//! store").
//!
//! The cache keeps one invariant, the same one the server's store keeps: a
//! tree or commit in the store has everything it reaches in the store too.
//! So a fill stops at any object already held, and writes the trees and
//! commits it fetched only after their children, deepest first. A fill cut
//! short leaves only blobs and complete trees behind, never a tree with a
//! hole, and the next fill finishes the job.

use super::session::LinkSlot;
use super::upstream::UpstreamLink;
use rosen_core::{FileMode, Object, ObjectId, ObjectKind};
use rosen_protocol::{
    ErrorBody, ErrorCode, ObjectBatch, ObjectQuery, Outcome, Request, Response, WireObject,
};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use std::collections::HashSet;

/// IDs per fetch request. Small enough that the request frame stays small;
/// the server also defers objects past its byte budget.
const IDS_PER_FETCH: usize = 1024;

/// Makes everything reachable from `root` readable from `store`, fetching
/// what is missing from the server. Returns how many objects were fetched.
/// The server is only contacted when something is missing.
pub async fn ensure(
    store: &LooseObjectStore,
    links: &LinkSlot,
    root: ObjectId,
) -> Result<u64, ErrorBody> {
    let mut walk = Walk::default();
    walk.want(vec![root]);
    let (mut wanted, mut walk) = blocking(store, |store, walk| walk.drop_held(store), walk).await?;
    if wanted.is_empty() {
        return Ok(0);
    }
    let link = links.get().await?;
    while !wanted.is_empty() {
        let batch = fetch_all(&link, &wanted).await?;
        (wanted, walk) =
            blocking(store, move |store, walk| walk.absorb(store, batch), walk).await?;
    }
    let fetched = walk.fetched;
    blocking(store, |store, walk| walk.write_deferred(store), walk).await?;
    Ok(fetched)
}

/// Answers a client's `GetObjects` on a cache: from the local store, then
/// from the server for the rest. Fetched blobs are kept; trees and commits
/// are not, because their children may not be here (see the module docs).
pub async fn get_objects(
    store: &LooseObjectStore,
    links: &LinkSlot,
    query: ObjectQuery,
) -> Result<ObjectBatch, ErrorBody> {
    let local_store = store.clone();
    let mut local =
        run_blocking(move || crate::objects::read_batch(&local_store, &query.ids)).await?;
    if local.missing.is_empty() {
        return Ok(local);
    }
    let link = links.get().await?;
    let remote = fetch(&link, std::mem::take(&mut local.missing)).await?;
    let blobs: Vec<Vec<u8>> = remote
        .objects
        .iter()
        .filter(|object| ObjectKind::from(object.kind) == ObjectKind::Blob)
        .map(|object| object.data.clone())
        .collect();
    let blob_store = store.clone();
    run_blocking(move || {
        blobs
            .iter()
            .try_for_each(|data| blob_store.put_raw(ObjectKind::Blob, data).map(|_| ()))
    })
    .await?;
    local.objects.extend(remote.objects);
    local.missing = remote.missing;
    Ok(local)
}

/// The state of one fill between fetch rounds.
#[derive(Default)]
struct Walk {
    /// Every ID this fill has asked for, so a shared subtree is fetched once.
    seen: HashSet<ObjectId>,
    /// IDs to fetch next.
    next: Vec<ObjectId>,
    /// Trees and commits fetched, parents before children.
    deferred: Vec<(ObjectKind, Vec<u8>)>,
    fetched: u64,
}

impl Walk {
    fn want(&mut self, ids: Vec<ObjectId>) {
        for id in ids {
            if self.seen.insert(id) {
                self.next.push(id);
            }
        }
    }

    /// Keeps only the wanted IDs the store lacks, and returns them.
    fn drop_held(&mut self, store: &LooseObjectStore) -> Result<Vec<ObjectId>, StoreError> {
        let mut missing = Vec::new();
        for id in std::mem::take(&mut self.next) {
            if !store.contains(&id)? {
                missing.push(id);
            }
        }
        Ok(missing)
    }

    /// Stores fetched blobs at once, defers trees and commits, and returns
    /// the children still missing.
    fn absorb(
        &mut self,
        store: &LooseObjectStore,
        objects: Vec<WireObject>,
    ) -> Result<Vec<ObjectId>, StoreError> {
        for object in objects {
            self.fetched += 1;
            let kind = ObjectKind::from(object.kind);
            if kind == ObjectKind::Blob {
                store.put_raw(kind, &object.data)?;
                continue;
            }
            let children = match Object::decode_body(kind, &object.data) {
                Ok(Object::Tree(tree)) => tree
                    .entries()
                    .iter()
                    // Gitlinks name commits in other repositories; no store holds them.
                    .filter(|entry| entry.mode != FileMode::Submodule)
                    .map(|entry| entry.id)
                    .collect(),
                Ok(Object::Commit(commit)) => vec![commit.tree],
                Ok(Object::Blob(_)) => Vec::new(),
                Err(err) => {
                    let id = ObjectId::hash_object(kind.as_str(), &object.data);
                    return Err(StoreError::Malformed(id, err));
                }
            };
            self.want(children);
            self.deferred.push((kind, object.data));
        }
        self.drop_held(store)
    }

    /// Writes deferred trees and commits deepest first, so each is written
    /// only after everything it names.
    fn write_deferred(&mut self, store: &LooseObjectStore) -> Result<(), StoreError> {
        for (kind, data) in self.deferred.iter().rev() {
            store.put_raw(*kind, data)?;
        }
        Ok(())
    }
}

/// Fetches every object in `ids`, asking again for any the server deferred.
/// Every object returned is checked against the asked IDs by its hash.
async fn fetch_all(link: &UpstreamLink, ids: &[ObjectId]) -> Result<Vec<WireObject>, ErrorBody> {
    let mut objects = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(IDS_PER_FETCH) {
        let mut remaining: Vec<ObjectId> = chunk.to_vec();
        while !remaining.is_empty() {
            let batch = fetch(link, remaining.clone()).await?;
            if let Some(id) = batch.missing.first() {
                let message = format!("the server does not hold {id}");
                return Err(ErrorBody::new(ErrorCode::NotFound, message));
            }
            if batch.objects.is_empty() {
                return Err(protocol_error("the server answered a fetch with nothing"));
            }
            let got: HashSet<ObjectId> = batch.objects.iter().map(wire_id).collect();
            if got.iter().any(|id| !remaining.contains(id)) {
                return Err(protocol_error(
                    "the server sent an object that was not asked for",
                ));
            }
            remaining.retain(|id| !got.contains(id));
            objects.extend(batch.objects);
        }
    }
    Ok(objects)
}

async fn fetch(link: &UpstreamLink, ids: Vec<ObjectId>) -> Result<ObjectBatch, ErrorBody> {
    match link.request(Request::GetObjects(ObjectQuery { ids })).await {
        Outcome::Ok(Response::Objects(batch)) => Ok(batch),
        Outcome::Ok(other) => Err(protocol_error(&format!("unexpected answer {other:?}"))),
        Outcome::Error(error) => Err(error),
    }
}

fn wire_id(object: &WireObject) -> ObjectId {
    ObjectId::hash_object(ObjectKind::from(object.kind).as_str(), &object.data)
}

fn protocol_error(message: &str) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, message)
}

/// Runs one step of a walk on the blocking pool, handing the walk back.
async fn blocking<T: Send + 'static>(
    store: &LooseObjectStore,
    step: impl FnOnce(&LooseObjectStore, &mut Walk) -> Result<T, StoreError> + Send + 'static,
    mut walk: Walk,
) -> Result<(T, Walk), ErrorBody> {
    let store = store.clone();
    run_blocking(move || step(&store, &mut walk).map(|value| (value, walk))).await
}

async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, ErrorBody> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(ErrorBody::new(ErrorCode::Internal, err.to_string())),
        Err(join) => Err(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}
