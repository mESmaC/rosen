//! Write-through snapshots (SPEC R2, D17): storing the objects a client
//! uploads, and recording a snapshot commit on a change.
//!
//! The client sends only what changed: the blobs of edited files and the
//! trees above them. A tree is stored only once every object it names is
//! stored, so any tree in the store is complete, and recording a snapshot
//! only has to check that its root tree is present.

use crate::changes::{self, ChangeError};
use crate::events::Staged;
use crate::ServerState;
use rosen_core::{
    Change, ChangeId, ChangeParent, Commit, FileMode, ObjectId, ObjectKind, Signature, Snapshot,
    SnapshotKind, Tree,
};
use rosen_protocol::{
    ErrorBody, ErrorCode, EventKind, ObjectUpload, ObjectsStored, Outcome, Response,
    SnapshotOutcome, SnapshotRequest, UploadKind,
};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use rusqlite::Connection;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("tree {tree} names {entry}, which the server does not hold; upload it first")]
    MissingEntry { tree: ObjectId, entry: ObjectId },
    #[error("uploaded tree is not valid git format: {0}")]
    MalformedTree(rosen_core::Error),
    #[error("the server does not hold tree {0}; upload it with put_objects first")]
    MissingTree(ObjectId),
    #[error(transparent)]
    Change(#[from] ChangeError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl SnapshotError {
    fn code(&self) -> ErrorCode {
        match self {
            SnapshotError::MissingEntry { .. }
            | SnapshotError::MalformedTree(_)
            | SnapshotError::MissingTree(_) => ErrorCode::InvalidArgument,
            SnapshotError::Change(err) => crate::rpc::change_error_code(err),
            SnapshotError::Store(StoreError::WrongKind { .. }) => ErrorCode::InvalidArgument,
            SnapshotError::Store(_) => ErrorCode::Internal,
        }
    }
}

/// Answers `Request::PutObjects`.
pub async fn put_objects(state: &Arc<ServerState>, upload: ObjectUpload) -> Outcome {
    on_blocking(state, move |state| {
        let store = LooseObjectStore::open(state.config.objects_dir())?;
        let ids = store_objects(&store, &upload)?;
        Ok(Response::ObjectsStored(ObjectsStored { ids }))
    })
    .await
}

/// Answers `Request::Snapshot`.
pub async fn snapshot(state: &Arc<ServerState>, request: SnapshotRequest) -> Outcome {
    on_blocking(state, move |state| {
        let store = LooseObjectStore::open(state.config.objects_dir())?;
        let outcome = record(state, &store, &request)?;
        if outcome.recorded {
            delivered(state);
        }
        Ok(Response::Snapshot(outcome))
    })
    .await
}

/// Store and SQLite I/O run on the blocking pool, so they never stall the
/// async workers that carry every other connection.
async fn on_blocking(
    state: &Arc<ServerState>,
    work: impl FnOnce(&ServerState) -> Result<Response, SnapshotError> + Send + 'static,
) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || work(&state)).await {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(err.code(), err.to_string())),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

/// Stores the batch's blobs, then its trees in order. A tree whose entries
/// are not all stored is refused, and so is every tree after it, so the
/// store never holds an incomplete tree.
///
/// Blobs name nothing, so they are stored first and in parallel: each loose
/// object is synced on its own, and on NTFS one sync at a time made a burst
/// of a few hundred files take seconds (#58). Trees are few, and each may
/// name an earlier one, so they stay in order.
pub fn store_objects(
    store: &(dyn ObjectStore + Sync),
    upload: &ObjectUpload,
) -> Result<Vec<ObjectId>, SnapshotError> {
    let blobs: Vec<&[u8]> = upload
        .objects
        .iter()
        .filter(|object| object.kind == UploadKind::Blob)
        .map(|object| object.data.as_slice())
        .collect();
    store_blobs(store, &blobs)?;
    let mut ids = Vec::with_capacity(upload.objects.len());
    for object in &upload.objects {
        let id = match object.kind {
            UploadKind::Blob => ObjectId::hash_object("blob", &object.data),
            UploadKind::Tree => {
                let id = ObjectId::hash_object("tree", &object.data);
                let tree = match rosen_core::Object::decode_body(ObjectKind::Tree, &object.data) {
                    Ok(rosen_core::Object::Tree(tree)) => tree,
                    Ok(_) => unreachable!("decoding as a tree yields a tree"),
                    Err(err) => return Err(SnapshotError::MalformedTree(err)),
                };
                ensure_complete(store, id, &tree)?;
                store.put_raw(ObjectKind::Tree, &object.data)?
            }
        };
        ids.push(id);
    }
    Ok(ids)
}

/// More writers than this only queue behind the file system.
const MAX_BLOB_WRITERS: usize = 16;

/// Stores `blobs` on up to [`MAX_BLOB_WRITERS`] threads, returning the first
/// failure. Every blob that succeeded is durable either way.
fn store_blobs(store: &(dyn ObjectStore + Sync), blobs: &[&[u8]]) -> Result<(), SnapshotError> {
    let writers = blobs.len().min(MAX_BLOB_WRITERS);
    if writers <= 1 {
        for data in blobs {
            store.put_raw(ObjectKind::Blob, data)?;
        }
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let failure = Mutex::new(None);
    let writer = || {
        while let Some(data) = blobs.get(next.fetch_add(1, Ordering::Relaxed)) {
            if let Err(err) = store.put_raw(ObjectKind::Blob, data) {
                failure
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get_or_insert(err);
                // Skip the rest: the batch fails, and the client resends it.
                next.store(blobs.len(), Ordering::Relaxed);
            }
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..writers {
            scope.spawn(writer);
        }
        writer();
    });
    match failure.into_inner().unwrap_or_else(|p| p.into_inner()) {
        Some(err) => Err(err.into()),
        None => Ok(()),
    }
}

/// Every entry but a gitlink must be stored. Gitlinks name commits in other
/// repositories, which Rosen never holds. The kind of each entry is not
/// read back: that would decompress every child, and a mislabelled entry
/// only makes the client's own snapshot unreadable.
fn ensure_complete(
    store: &dyn ObjectStore,
    id: ObjectId,
    tree: &Tree,
) -> Result<(), SnapshotError> {
    for entry in tree.entries() {
        if entry.mode != FileMode::Submodule && !store.contains(&entry.id)? {
            return Err(SnapshotError::MissingEntry {
                tree: id,
                entry: entry.id,
            });
        }
    }
    Ok(())
}

/// Writes the snapshot commit and appends it to the change, holding the
/// registry's connection throughout, so two snapshots of one change can never
/// both pick the same parent commit. Also returns the paths whose conflicts
/// the snapshot stored.
fn record(
    state: &ServerState,
    store: &dyn ObjectStore,
    request: &SnapshotRequest,
) -> Result<SnapshotOutcome, SnapshotError> {
    if request.tree != empty_tree() && !store.contains(&request.tree)? {
        return Err(SnapshotError::MissingTree(request.tree));
    }
    let ctx = crate::history::open(state).map_err(history_failure)?;
    let outcome = state.changes.with_conn(|conn| {
        let current = changes::get(conn, changes::resolve(conn, &request.change)?)?;
        let head = merge_snapshot(&ctx, &current.change, request)?;
        record_at(conn, store, request, &current.change, head)
    })?;
    Ok(outcome)
}

/// Records while the caller holds the registry connection, allowing a tree
/// edit and its snapshot to use the same head without losing concurrent writes.
pub(crate) fn record_on(
    conn: &mut Connection,
    store: &dyn ObjectStore,
    request: &SnapshotRequest,
) -> Result<SnapshotOutcome, ChangeError> {
    let id = changes::resolve(conn, &request.change)?;
    let current = changes::get(conn, id)?;
    record_at(
        conn,
        store,
        request,
        &current.change,
        MergedHead::clean(request.tree),
    )
}

fn record_at(
    conn: &mut Connection,
    store: &dyn ObjectStore,
    request: &SnapshotRequest,
    expected: &Change,
    head: MergedHead,
) -> Result<SnapshotOutcome, ChangeError> {
    let id = expected.id;
    let current = changes::get(conn, id)?;
    if request.message.is_none() && holds_tree(&current.change, request) {
        let commit = current.change.snapshots.last().map(|s| s.commit);
        return Ok(SnapshotOutcome {
            change: current,
            commit,
            recorded: false,
        });
    }
    let commit = snapshot_commit(conn, &current.change, request)?;
    let commit_id = store.put_commit(&commit).map_err(store_failure)?;
    let snapshot = Snapshot {
        commit: commit_id,
        tree: request.tree,
        kind: request
            .message
            .clone()
            .map_or(SnapshotKind::Automatic, SnapshotKind::Named),
    };
    let events = events(id, commit_id, request, &head.conflicts);
    Ok(SnapshotOutcome {
        change: changes::record_snapshot_merged(
            conn,
            expected,
            snapshot,
            head.tree,
            head.conflicts,
            &events,
        )?,
        commit: Some(commit_id),
        recorded: true,
    })
}

/// The events a recorded snapshot reports, staged with it (R9): `snapshot`,
/// and `conflict` naming the files a pending rebase left markers in.
fn events(
    change: ChangeId,
    commit: ObjectId,
    request: &SnapshotRequest,
    conflicts: &Regions,
) -> Vec<Staged> {
    let detail = serde_json::json!({
        "commit": commit,
        "tree": request.tree,
        "named": request.message.is_some(),
    });
    let mut events = vec![Staged::about(EventKind::Snapshot, change, detail)];
    let paths: Vec<&String> = conflicts.iter().map(|(path, _)| path).collect();
    if !paths.is_empty() {
        let detail = serde_json::json!({ "paths": paths });
        events.push(Staged::about(EventKind::Conflict, change, detail));
    }
    events
}

/// An automatic snapshot of the tree the change already holds stores
/// nothing: the watcher may resend after a lost answer, or after a write
/// that put a file back as it was. Before the owner applies a server
/// rebase, the head is the merge of the latest snapshot, so a resend of
/// that snapshot's tree from the old basis is held too.
fn holds_tree(change: &Change, request: &SnapshotRequest) -> bool {
    change.head == request.tree || resends_latest(change, request)
}

/// The workspace has not applied the head yet and names the latest
/// snapshot's tree, whose edits the head already holds.
fn resends_latest(change: &Change, request: &SnapshotRequest) -> bool {
    request.from.is_some_and(|from| from != change.head)
        && change
            .snapshots
            .last()
            .is_some_and(|s| s.tree == request.tree)
}

fn snapshot_commit(
    conn: &Connection,
    change: &Change,
    request: &SnapshotRequest,
) -> Result<Commit, ChangeError> {
    let parent = match change.snapshots.last() {
        Some(latest) => Some(latest.commit),
        None => first_parent(conn, change.parent)?,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let owner = change.owner.as_str();
    let mut message = request
        .message
        .clone()
        .unwrap_or_else(|| "Automatic snapshot".to_owned());
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(Commit {
        tree: request.tree,
        parents: parent.into_iter().collect(),
        author: Signature::new(owner, format!("{owner}@rosen"), now),
        committer: Signature::new("rosen-server", "rosen-server@rosen", now),
        extra_headers: Vec::new(),
        message: message.into_bytes(),
    })
}

/// A change's first snapshot follows what it was started from: the base
/// commit, or the newest commit of the change below it in a stack.
fn first_parent(conn: &Connection, parent: ChangeParent) -> Result<Option<ObjectId>, ChangeError> {
    let mut cursor = parent;
    let mut seen = std::collections::HashSet::<ChangeId>::new();
    loop {
        match cursor {
            ChangeParent::Base(commit) => return Ok(Some(commit)),
            ChangeParent::Change(below) => {
                // A damaged store could hold a loop; stop rather than spin.
                if !seen.insert(below) {
                    return Ok(None);
                }
                let below = changes::get(conn, below)?.change;
                if let Some(latest) = below.snapshots.last() {
                    return Ok(Some(latest.commit));
                }
                cursor = below.parent;
            }
        }
    }
}

/// Stores the events a recorded snapshot staged, then refreshes the stacks
/// its new head may move. Call after the snapshot's transaction commits.
pub(crate) fn delivered(state: &ServerState) {
    state.events.deliver_staged();
    crate::stacks::refresh(state);
}

fn empty_tree() -> ObjectId {
    ObjectId::hash_object("tree", b"")
}

fn store_failure(err: StoreError) -> ChangeError {
    ChangeError::Io(std::io::Error::other(err.to_string()))
}

type Regions = Vec<(String, Vec<rosen_core::Conflict>)>;

/// The head a snapshot gives its change, and the conflicts stored with it.
struct MergedHead {
    tree: ObjectId,
    conflicts: Regions,
}

impl MergedHead {
    fn clean(tree: ObjectId) -> Self {
        MergedHead {
            tree,
            conflicts: Vec::new(),
        }
    }
}

// Local snapshot commits retain exactly the author's tree. Only the change's
// live head incorporates the server rebase, until its owner applies it.
//
// Edits that conflict with the rebase are stored, not refused (D2, R5):
// refusing left the workspace stuck, because applying the rebase needs
// every local edit stored first (#105). The head takes diff3 markers with
// the snapshot's side as `ours`, as a conflicting rebase writes them, and
// the conflicts are stored open, so the owner applies the head, keeps both
// sides and runs `rosen resolve`.
fn merge_snapshot(
    ctx: &crate::history::Ctx,
    current: &Change,
    request: &SnapshotRequest,
) -> Result<MergedHead, ChangeError> {
    let from = request.from.ok_or_else(|| {
        ChangeError::Base("snapshot must name its workspace basis; update the client".into())
    })?;
    if from == current.head {
        return Ok(MergedHead::clean(request.tree));
    }
    // Merged again, a resend would mark its own edits as a conflict.
    if resends_latest(current, request) {
        return Ok(MergedHead::clean(current.head));
    }
    let sides = [from, current.head, request.tree];
    let merged =
        crate::history::merge::merge_trees_marked(ctx, sides, &|_| rosen_merge::MergePolicy::Text)
            .map_err(|err| ChangeError::Io(std::io::Error::other(err.to_string())))?;
    let Some(tree) = merged.tree else {
        let paths = merged.conflicts.join(", ");
        return Err(ChangeError::Refused(rosen_core::Error::InvalidPath(
            format!(
                "{paths}: the snapshot and the pending rebase need a file and a directory \
             at one path; local edits kept"
            ),
        )));
    };
    let conflicts = merged
        .conflicts
        .iter()
        .map(|path| {
            crate::landing::conflict_routes::regions(
                &ctx.store,
                [from, request.tree, current.head],
                path,
            )
            .map_err(|err| ChangeError::Io(std::io::Error::other(err.to_string())))
        })
        .collect::<Result<Regions, _>>()?;
    Ok(MergedHead { tree, conflicts })
}

fn history_failure(err: crate::history::HistoryError) -> SnapshotError {
    SnapshotError::Change(ChangeError::Io(std::io::Error::other(err.to_string())))
}
