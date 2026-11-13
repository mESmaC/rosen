//! Write-through snapshots (SPEC R2, D17): storing the objects a client
//! uploads, and recording a snapshot commit on a change.
//!
//! The client sends only what changed: the blobs of edited files and the
//! trees above them. A tree is stored only once every object it names is
//! stored, so any tree in the store is complete, and recording a snapshot
//! only has to check that its root tree is present.

use crate::changes::{self, ChangeError};
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
use std::sync::Arc;
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
            publish(state, &outcome, request.message.is_some());
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

/// Stores the batch in order. A tree whose entries are not all stored is
/// refused, and so is everything after it, so the store never holds an
/// incomplete tree.
pub fn store_objects(
    store: &dyn ObjectStore,
    upload: &ObjectUpload,
) -> Result<Vec<ObjectId>, SnapshotError> {
    let mut ids = Vec::with_capacity(upload.objects.len());
    for object in &upload.objects {
        let id = match object.kind {
            UploadKind::Blob => store.put_raw(ObjectKind::Blob, &object.data)?,
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
/// both pick the same parent commit.
fn record(
    state: &ServerState,
    store: &dyn ObjectStore,
    request: &SnapshotRequest,
) -> Result<SnapshotOutcome, SnapshotError> {
    if request.tree != empty_tree() && !store.contains(&request.tree)? {
        return Err(SnapshotError::MissingTree(request.tree));
    }
    let ctx = crate::history::open(state).map_err(history_failure)?;
    let mut outcome = None;
    state.changes.with_conn(|conn| {
        let current = changes::get(conn, changes::resolve(conn, &request.change)?)?;
        let head = merge_snapshot(&ctx, &current.change, request)?;
        outcome = Some(record_at(conn, store, request, &current.change, head)?);
        Ok(())
    })?;
    Ok(outcome.expect("set by every successful path above"))
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
    record_at(conn, store, request, &current.change, request.tree)
}

fn record_at(
    conn: &mut Connection,
    store: &dyn ObjectStore,
    request: &SnapshotRequest,
    expected: &Change,
    head: ObjectId,
) -> Result<SnapshotOutcome, ChangeError> {
    let id = expected.id;
    let current = changes::get(conn, id)?;
    if request.message.is_none() && holds_tree(&current.change, request.tree) {
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
    Ok(SnapshotOutcome {
        change: changes::record_snapshot_merged(conn, expected, snapshot, head)?,
        commit: Some(commit_id),
        recorded: true,
    })
}

/// An automatic snapshot of the tree the change already holds stores
/// nothing: the watcher may resend after a lost answer, or after a write
/// that put a file back as it was.
fn holds_tree(change: &Change, tree: ObjectId) -> bool {
    change.head == tree
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

pub(crate) fn publish(state: &ServerState, outcome: &SnapshotOutcome, named: bool) {
    let change = &outcome.change.change;
    let detail = serde_json::json!({
        "commit": outcome.commit,
        "tree": change.snapshots.last().map(|s| s.tree),
        "named": named,
    });
    state
        .events
        .publish(EventKind::Snapshot, Some(change.id.to_string()), detail);
}

fn empty_tree() -> ObjectId {
    ObjectId::hash_object("tree", b"")
}

fn store_failure(err: StoreError) -> ChangeError {
    ChangeError::Io(std::io::Error::other(err.to_string()))
}

// Local snapshot commits retain exactly the author's tree. Only the change's
// live head incorporates the server rebase, until its owner applies it.
fn merge_snapshot(
    ctx: &crate::history::Ctx,
    current: &Change,
    request: &SnapshotRequest,
) -> Result<ObjectId, ChangeError> {
    let from = request.from.ok_or_else(|| {
        ChangeError::Base("snapshot must name its workspace basis; update the client".into())
    })?;
    if from == current.head {
        return Ok(request.tree);
    }
    let merged =
        crate::history::merge::merge_trees(ctx, [from, current.head, request.tree], &|_| {
            rosen_merge::MergePolicy::Text
        })
        .map_err(|err| ChangeError::Io(std::io::Error::other(err.to_string())))?;
    merged.tree.ok_or_else(|| {
        ChangeError::Base(format!(
            "snapshot conflicts with pending rebase in {}; local edits kept",
            merged.conflicts.join(", ")
        ))
    })
}

fn history_failure(err: crate::history::HistoryError) -> SnapshotError {
    SnapshotError::Change(ChangeError::Io(std::io::Error::other(err.to_string())))
}
