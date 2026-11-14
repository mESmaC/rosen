//! Filling a cache daemon's store from the server (SPEC D8, §3 "Object
//! store").
//!
//! The cache keeps the server's invariant, a tree or commit in the store has
//! everything it reaches in the store too, with one exception: a sparse
//! workspace (R3) needs every tree but only the blobs under its chosen paths,
//! so a sparse fill stores trees that lack blobs and records each in
//! [`PartialObjects`] before writing it. Every stored tree still has all its
//! subtrees stored. So a fill stops at any object held and not recorded,
//! walks a recorded one from the local store, and writes the trees and
//! commits it fetched only after every fetched object they name. A fill cut
//! short leaves only blobs, complete trees and recorded trees behind, never
//! a tree missing a subtree, and the next fill finishes the job.

use super::partial::PartialObjects;
use super::session::LinkSlot;
use super::upstream::UpstreamLink;
use rosen_core::{FileMode, Object, ObjectId, ObjectKind};
use rosen_protocol::{
    ErrorBody, ErrorCode, ObjectBatch, ObjectChunkQuery, ObjectQuery, Outcome, Request, Response,
    WireObject, OBJECT_CHUNK_BYTES,
};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// IDs per fetch request. Small enough that the request frame stays small;
/// the server also defers objects past its byte budget.
const IDS_PER_FETCH: usize = 1024;

/// Makes everything reachable from `root` readable from `store`, fetching
/// what is missing from the server, or with `sparse` paths only every tree
/// and the blobs at or under those paths. Returns how many objects were
/// fetched. The server is only contacted when something is missing.
pub async fn ensure(
    store: &LooseObjectStore,
    partial: &Arc<PartialObjects>,
    links: &LinkSlot,
    root: ObjectId,
    sparse: &[String],
) -> Result<u64, ErrorBody> {
    let mut walk = Walk::new(Selection::new(sparse), Arc::clone(partial));
    let need = walk.selection.need_at(b"");
    walk.want(root, need);
    let (mut wanted, mut walk) = blocking(store, |store, walk| walk.resolve(store), walk).await?;
    if !wanted.is_empty() {
        let link = links.get().await?;
        while !wanted.is_empty() {
            let batch = fetch_all(&link, &wanted).await?;
            (wanted, walk) =
                blocking(store, move |store, walk| walk.absorb(store, batch), walk).await?;
        }
    }
    let fetched = walk.fetched;
    // Runs even when nothing was fetched: a recorded tree walked locally may
    // have become complete, and its record is then dropped.
    blocking(store, |store, walk| walk.finish(store), walk).await?;
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
    // The client reads these in slices, which this daemon forwards.
    local.large.extend(remote.large);
    Ok(local)
}

/// The paths a sparse workspace holds, as the client's `Sparse` chooses
/// them: a chosen path holds itself and everything under it.
struct Selection {
    /// `/`-separated paths, or empty for the whole tree.
    paths: Vec<Vec<u8>>,
}

impl Selection {
    fn new(sparse: &[String]) -> Self {
        let paths: Vec<Vec<u8>> = sparse
            .iter()
            .map(|path| {
                let parts: Vec<&str> = path
                    .split('/')
                    .filter(|part| !part.is_empty() && *part != ".")
                    .collect();
                parts.join("/").into_bytes()
            })
            .collect();
        // A path naming the root chooses the whole tree.
        if paths.iter().any(Vec::is_empty) {
            return Self { paths: Vec::new() };
        }
        Self { paths }
    }

    fn includes(&self, path: &[u8]) -> bool {
        self.paths.is_empty() || self.paths.iter().any(|chosen| is_within(path, chosen))
    }

    /// What the workspace needs of the tree at `path`.
    fn need_at(&self, path: &[u8]) -> Need {
        if self.includes(path) {
            Need::Full
        } else if path.is_empty() || self.paths.iter().any(|chosen| is_within(chosen, path)) {
            Need::Toward(path.to_vec())
        } else {
            Need::TreesOnly
        }
    }
}

/// Whether `path` is `ancestor` or lies under it.
fn is_within(path: &[u8], ancestor: &[u8]) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.is_empty() || rest[0] == b'/')
}

/// What a fill needs of one object. Every tree is always needed, because a
/// sparse workspace's snapshots and tree cache read the whole tree (R2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Need {
    /// The object and everything it reaches.
    Full,
    /// The tree at this path, on the way to a chosen path: every subtree,
    /// and the blobs the selection includes.
    Toward(Vec<u8>),
    /// A tree with no chosen path at or under it: every subtree, no blobs.
    TreesOnly,
}

/// A tree or commit this fill read, from the server or the local store.
struct Node {
    kind: ObjectKind,
    data: Vec<u8>,
    /// Fetched, so written at the end; otherwise a recorded partial object
    /// already in the store.
    fetched: bool,
    /// For a commit, its tree, named by the empty string.
    links: Vec<Link>,
}

struct Link {
    name: Vec<u8>,
    id: ObjectId,
    is_tree: bool,
}

/// The state of one fill between fetch rounds.
struct Walk {
    selection: Selection,
    partial: Arc<PartialObjects>,
    /// Every visit this fill has queued, so a shared subtree is visited once
    /// for each need.
    seen: HashSet<(ObjectId, Need)>,
    /// Visits to resolve next.
    next: Vec<(ObjectId, Need)>,
    /// Objects asked of the server, with the needs to apply on arrival.
    fetching: HashMap<ObjectId, Vec<Need>>,
    nodes: HashMap<ObjectId, Node>,
    /// Objects known stored with their whole closure: blobs fetched or found,
    /// and trees found stored and not recorded as partial.
    complete: HashSet<ObjectId>,
    fetched: u64,
}

impl Walk {
    fn new(selection: Selection, partial: Arc<PartialObjects>) -> Self {
        Self {
            selection,
            partial,
            seen: HashSet::new(),
            next: Vec::new(),
            fetching: HashMap::new(),
            nodes: HashMap::new(),
            complete: HashSet::new(),
            fetched: 0,
        }
    }

    fn want(&mut self, id: ObjectId, need: Need) {
        // A full visit covers every other.
        if self.seen.contains(&(id, Need::Full)) {
            return;
        }
        if self.seen.insert((id, need.clone())) {
            self.next.push((id, need));
        }
    }

    /// Resolves queued visits against the store, walking recorded partial
    /// objects locally, and returns the IDs to fetch.
    fn resolve(&mut self, store: &LooseObjectStore) -> Result<Vec<ObjectId>, StoreError> {
        while let Some((id, need)) = self.next.pop() {
            if self.nodes.contains_key(&id) {
                self.expand(id, &need);
            } else if self.complete.contains(&id) {
                continue;
            } else if let Some(needs) = self.fetching.get_mut(&id) {
                needs.push(need);
            } else if !store.contains(&id)? {
                self.fetching.insert(id, vec![need]);
            } else if !self.partial.contains(&id) {
                // Asked after the store: a record is made before its object
                // is written, so a stored object's record is already visible.
                self.complete.insert(id);
            } else if need != Need::TreesOnly {
                // A partial tree holds all its subtrees, so only a visit that
                // wants blobs reads it.
                let raw = store.get_raw(&id)?.ok_or(StoreError::NotFound(id))?;
                self.add_node(id, raw.kind, raw.data, false)?;
                self.expand(id, &need);
            }
        }
        Ok(self.fetching.keys().copied().collect())
    }

    /// Stores fetched blobs at once, keeps trees and commits to write at the
    /// end, and returns what is still to fetch.
    fn absorb(
        &mut self,
        store: &LooseObjectStore,
        objects: Vec<WireObject>,
    ) -> Result<Vec<ObjectId>, StoreError> {
        for object in objects {
            self.fetched += 1;
            let kind = ObjectKind::from(object.kind);
            let id = ObjectId::hash_object(kind.as_str(), &object.data);
            let needs = self.fetching.remove(&id).unwrap_or_default();
            if kind == ObjectKind::Blob {
                store.put_raw(kind, &object.data)?;
                self.complete.insert(id);
                continue;
            }
            self.add_node(id, kind, object.data, true)?;
            for need in needs {
                self.expand(id, &need);
            }
        }
        self.resolve(store)
    }

    fn add_node(
        &mut self,
        id: ObjectId,
        kind: ObjectKind,
        data: Vec<u8>,
        fetched: bool,
    ) -> Result<(), StoreError> {
        let links = match Object::decode_body(kind, &data) {
            Ok(Object::Tree(tree)) => tree
                .entries()
                .iter()
                // Gitlinks name commits in other repositories; no store holds them.
                .filter(|entry| entry.mode != FileMode::Submodule)
                .map(|entry| Link {
                    name: entry.name.clone(),
                    id: entry.id,
                    is_tree: entry.mode.is_tree(),
                })
                .collect(),
            Ok(Object::Commit(commit)) => vec![Link {
                name: Vec::new(),
                id: commit.tree,
                is_tree: true,
            }],
            Ok(Object::Blob(_)) => Vec::new(),
            Err(err) => return Err(StoreError::Malformed(id, err)),
        };
        let node = Node {
            kind,
            data,
            fetched,
            links,
        };
        self.nodes.insert(id, node);
        Ok(())
    }

    /// Queues what `need` wants of the node's children.
    fn expand(&mut self, id: ObjectId, need: &Need) {
        let node = &self.nodes[&id];
        let is_commit = node.kind == ObjectKind::Commit;
        let wanted: Vec<(ObjectId, Need)> = node
            .links
            .iter()
            .filter_map(|link| {
                let child = match need {
                    Need::Full => Need::Full,
                    Need::TreesOnly if link.is_tree => Need::TreesOnly,
                    Need::TreesOnly => return None,
                    Need::Toward(dir) => {
                        // A commit's tree stands at the commit's own path.
                        let path = if is_commit {
                            dir.clone()
                        } else {
                            join(dir, &link.name)
                        };
                        if link.is_tree {
                            self.selection.need_at(&path)
                        } else if self.selection.includes(&path) {
                            Need::Full
                        } else {
                            return None;
                        }
                    }
                };
                Some((link.id, child))
            })
            .collect();
        for (child, need) in wanted {
            self.want(child, need);
        }
    }

    /// Writes fetched trees and commits, each after every node it names.
    /// Those whose closures are incomplete are recorded first; recorded
    /// nodes now complete have their records dropped last, once everything
    /// they reach is written.
    fn finish(&mut self, store: &LooseObjectStore) -> Result<(), StoreError> {
        let order = self.post_order();
        let mut incomplete = Vec::new();
        for id in &order {
            let node = &self.nodes[id];
            if node
                .links
                .iter()
                .all(|link| self.complete.contains(&link.id))
            {
                self.complete.insert(*id);
            } else if node.fetched {
                incomplete.push(*id);
            }
        }
        self.partial.mark(&incomplete)?;
        for id in &order {
            let node = &self.nodes[id];
            if node.fetched {
                store.put_raw(node.kind, &node.data)?;
            }
        }
        let completed: Vec<ObjectId> = order
            .into_iter()
            .filter(|id| self.complete.contains(id))
            .collect();
        self.partial.clear(&completed)?;
        Ok(())
    }

    /// Every node, each after every node it names. Fetch order is not
    /// enough, because a subtree shared by a parent and its sibling is
    /// fetched in the parent's round but named by the sibling too. Objects
    /// are content addressed, so the graph has no cycles.
    fn post_order(&self) -> Vec<ObjectId> {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut placed: HashSet<ObjectId> = HashSet::with_capacity(self.nodes.len());
        for &start in self.nodes.keys() {
            // Each frame is a node and how many of its links are done.
            let mut stack = vec![(start, 0usize)];
            while let Some(frame) = stack.last_mut() {
                let (id, next_link) = *frame;
                if placed.contains(&id) {
                    stack.pop();
                    continue;
                }
                if let Some(link) = self.nodes[&id].links.get(next_link) {
                    frame.1 += 1;
                    if self.nodes.contains_key(&link.id) && !placed.contains(&link.id) {
                        stack.push((link.id, 0));
                    }
                    continue;
                }
                placed.insert(id);
                order.push(id);
                stack.pop();
            }
        }
        order
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(dir.len() + 1 + name.len());
    path.extend_from_slice(dir);
    path.push(b'/');
    path.extend_from_slice(name);
    path
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
            let mut fetched = batch.objects;
            for id in batch.large {
                if !remaining.contains(&id) {
                    return Err(protocol_error("the server listed an object not asked for"));
                }
                fetched.push(fetch_large(link, id).await?);
            }
            if fetched.is_empty() {
                return Err(protocol_error("the server answered a fetch with nothing"));
            }
            let got: HashSet<ObjectId> = fetched.iter().map(wire_id).collect();
            if got.iter().any(|id| !remaining.contains(id)) {
                return Err(protocol_error(
                    "the server sent an object that was not asked for",
                ));
            }
            remaining.retain(|id| !got.contains(id));
            objects.extend(fetched);
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

/// Reads an object too large for one frame slice by slice. The caller
/// checks the assembled bytes against the asked ID, as for any object.
async fn fetch_large(link: &UpstreamLink, id: ObjectId) -> Result<WireObject, ErrorBody> {
    let mut data = Vec::new();
    loop {
        let query = ObjectChunkQuery {
            id,
            offset: data.len() as u64,
            len: OBJECT_CHUNK_BYTES as u64,
        };
        let chunk = match link.request(Request::GetObjectChunk(query)).await {
            Outcome::Ok(Response::ObjectChunk(chunk)) => chunk,
            Outcome::Ok(other) => {
                return Err(protocol_error(&format!("unexpected answer {other:?}")))
            }
            Outcome::Error(error) => return Err(error),
        };
        if chunk.data.is_empty() && (data.len() as u64) < chunk.total {
            return Err(protocol_error("the server sent an empty slice mid-object"));
        }
        data.extend_from_slice(&chunk.data);
        if data.len() as u64 >= chunk.total {
            return Ok(WireObject {
                kind: chunk.kind,
                data,
            });
        }
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
