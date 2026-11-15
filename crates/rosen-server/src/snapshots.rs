//! Write-through snapshots (SPEC R2, D17): storing the objects a client
//! uploads, and recording a snapshot commit on a change.
//!
//! The client sends only what changed: the blobs of edited files and the
//! trees above them. A tree is stored only once every object it names is
//! stored. Recording also checks the stored closure for later disk damage,
//! so the client can repair objects that its incremental upload omitted.

mod integrity;
mod resolution;

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
    #[error("snapshot objects are missing or damaged; resend {0:?} before retrying")]
    ObjectsNeeded(Vec<ObjectId>),
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
            SnapshotError::ObjectsNeeded(_) => ErrorCode::Internal,
            SnapshotError::MissingEntry { .. }
            | SnapshotError::MalformedTree(_)
            | SnapshotError::MissingTree(_) => ErrorCode::InvalidArgument,
            SnapshotError::Change(err) => crate::rpc::change_error_code(err),
            SnapshotError::Store(StoreError::WrongKind { .. }) => ErrorCode::InvalidArgument,
            SnapshotError::Store(_) => ErrorCode::Internal,
        }
    }

    fn body(&self) -> ErrorBody {
        let mut body = ErrorBody::new(self.code(), self.to_string());
        if let Self::ObjectsNeeded(ids) = self {
            body.objects_needed.clone_from(ids);
        }
        match self {
            Self::MissingEntry { entry, .. } => body.objects_needed.push(*entry),
            Self::MissingTree(id) | Self::Store(StoreError::Corrupt { id, .. }) => {
                body.objects_needed.push(*id);
            }
            _ => {}
        }
        body
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
        Ok(Err(err)) => Outcome::Error(err.body()),
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

/// How often a snapshot is prepared again because its change moved meanwhile.
/// Only another snapshot of the same change or a rebase of it moves it, so
/// more tries than this mean something writes it in a loop.
const RECORD_ATTEMPTS: u32 = 8;

/// Writes the snapshot commit and appends it to the change.
///
/// Every change operation of every agent shares the registry's connection,
/// so it is held only to read the change and, at the end, to append the
/// snapshot. The merge with a pending rebase and the write of the commit
/// object, which is synced to disk, run between the two with nothing held.
/// The append checks the change is still exactly as it was read, so two
/// snapshots of one change can never both pick the same parent commit: the
/// one that loses is prepared again from the new head. A commit object
/// written for a try that lost is never named by anything.
fn record(
    state: &ServerState,
    store: &LooseObjectStore,
    request: &SnapshotRequest,
) -> Result<SnapshotOutcome, SnapshotError> {
    integrity::check(store, request.tree)?;
    let ctx = crate::history::open(state).map_err(history_failure)?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let (current, parent, open_paths) = snapshot_inputs(state, request)?;
        if request.message.is_none()
            && holds_tree(&current.change, request)
            && (open_paths.is_empty() || current.change.head == request.tree)
        {
            return Ok(unrecorded(current));
        }
        let head = merge_snapshot(&ctx, &current.change, request, &open_paths)?;
        if request.message.is_none()
            && resends_latest(&current.change, request)
            && head.tree == current.change.head
            && head.conflicts.is_empty()
        {
            return Ok(unrecorded(current));
        }
        let commit = snapshot_commit(parent, &current.change, request);
        let commit_id = store.put_commit(&commit).map_err(store_failure)?;
        let snapshot = snapshot_of(commit_id, request);
        let events = events(current.change.id, commit_id, request, &head.conflicts);
        let appended = state.changes.with_conn(|conn| {
            let (tree, conflicts) = (head.tree, head.conflicts);
            changes::record_snapshot_merged(
                conn,
                &current.change,
                snapshot,
                tree,
                conflicts,
                &events,
            )
        });
        match appended {
            Ok(change) => {
                return Ok(SnapshotOutcome {
                    change,
                    commit: Some(commit_id),
                    recorded: true,
                });
            }
            Err(error) if changes::snapshot_moved(&error) && attempt < RECORD_ATTEMPTS => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn snapshot_inputs(
    state: &ServerState,
    request: &SnapshotRequest,
) -> Result<(rosen_protocol::ChangeRecord, Option<ObjectId>, Vec<String>), ChangeError> {
    state.changes.with_conn(|conn| {
        let current = changes::get(conn, changes::resolve(conn, &request.change)?)?;
        let parent = snapshot_parent(conn, &current.change)?;
        let open_paths = resolution::open_paths(conn, current.change.id)?;
        Ok((current, parent, open_paths))
    })
}

/// The answer to a snapshot of the tree the change already holds.
fn unrecorded(current: rosen_protocol::ChangeRecord) -> SnapshotOutcome {
    let commit = current.change.snapshots.last().map(|s| s.commit);
    SnapshotOutcome {
        change: current,
        commit,
        recorded: false,
    }
}

fn snapshot_of(commit: ObjectId, request: &SnapshotRequest) -> Snapshot {
    Snapshot {
        commit,
        tree: request.tree,
        kind: request
            .message
            .clone()
            .map_or(SnapshotKind::Automatic, SnapshotKind::Named),
    }
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
        return Ok(unrecorded(current));
    }
    let parent = snapshot_parent(conn, &current.change)?;
    let commit = snapshot_commit(parent, &current.change, request);
    let commit_id = store.put_commit(&commit).map_err(store_failure)?;
    let snapshot = snapshot_of(commit_id, request);
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

/// The commit a new snapshot of `change` follows.
fn snapshot_parent(conn: &Connection, change: &Change) -> Result<Option<ObjectId>, ChangeError> {
    match change.snapshots.last() {
        Some(latest) => Ok(Some(latest.commit)),
        None => first_parent(conn, change.parent),
    }
}

fn snapshot_commit(parent: Option<ObjectId>, change: &Change, request: &SnapshotRequest) -> Commit {
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
    Commit {
        tree: request.tree,
        parents: parent.into_iter().collect(),
        author: Signature::new(owner, format!("{owner}@rosen"), now),
        committer: Signature::new("rosen-server", "rosen-server@rosen", now),
        extra_headers: Vec::new(),
        message: message.into_bytes(),
    }
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
// New conflicts are stored, not refused (#105). Already open files instead
// take the snapshot's content verbatim: merging into their marked head would
// nest markers and make the owner's resolution impossible to store (#383).
fn merge_snapshot(
    ctx: &crate::history::Ctx,
    current: &Change,
    request: &SnapshotRequest,
    open_paths: &[String],
) -> Result<MergedHead, ChangeError> {
    let from = request.from.ok_or_else(|| {
        ChangeError::Base("snapshot must name its workspace basis; update the client".into())
    })?;
    if from == current.head {
        return Ok(MergedHead::clean(request.tree));
    }
    let sides = resolution::merge_sides(ctx, [from, current.head, request.tree], open_paths)?;
    // A resend still repairs open files, but never merges its own edits again.
    if resends_latest(current, request) {
        return Ok(MergedHead::clean(sides[1]));
    }
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
