//! Direct edits of immutable trees, with no server checkout (SPEC R3).
use crate::changes::ChangeError;
use crate::{changes, snapshots, ServerState};
use rosen_core::{FileMode, ObjectId, ObjectKind, Tree, TreeEntry};
use rosen_protocol::{
    ErrorBody, ErrorCode, Outcome, Response, SnapshotRequest, WorkspaceBlobWrite, WorkspaceWrite,
};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

/// One server-side file edit.
struct Edit {
    change: String,
    path: String,
    content: Option<Content>,
}

/// What a write puts at its path.
enum Content {
    /// Bytes sent with the request.
    Bytes(Vec<u8>),
    /// A blob already stored, as by a chunked upload (#243).
    Stored(ObjectId),
}

pub async fn write(state: &Arc<ServerState>, request: WorkspaceWrite) -> Outcome {
    let edit = Edit {
        change: request.change,
        path: request.path,
        content: request.data.map(|bytes| Content::Bytes(bytes.0)),
    };
    run(state, edit).await
}

pub async fn write_blob(state: &Arc<ServerState>, request: WorkspaceBlobWrite) -> Outcome {
    let edit = Edit {
        change: request.change,
        path: request.path,
        content: Some(Content::Stored(request.blob)),
    };
    run(state, edit).await
}

async fn run(state: &Arc<ServerState>, request: Edit) -> Outcome {
    let permit = match Arc::clone(&state.workspace_io).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return Outcome::Error(ErrorBody::new(
                ErrorCode::ShuttingDown,
                "workspace I/O closed",
            ))
        }
    };
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        edit(&state, request)
    })
    .await
    {
        Ok(Ok(answer)) => Outcome::Ok(Response::Snapshot(answer)),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(
            crate::rpc::change_error_code(&err),
            err.to_string(),
        )),
        Err(err) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, err.to_string())),
    }
}

fn edit(state: &ServerState, req: Edit) -> Result<rosen_protocol::SnapshotOutcome, ChangeError> {
    let parts = validate_path(&req.path)?;
    let store = LooseObjectStore::open(state.config.objects_dir()).map_err(store_error)?;
    // The blob is the one large object, and hashing, compressing and syncing
    // it is the slow part. Store it before taking the registry lock, which
    // every change operation on the server waits on (#78). Objects are
    // immutable, so a blob stored for an edit that is then refused is harmless.
    let blob = match &req.content {
        None => None,
        Some(Content::Bytes(bytes)) => Some(store.put_blob(bytes).map_err(store_error)?),
        Some(Content::Stored(id)) => Some(stored_blob(&store, *id)?),
    };
    // Keep the connection through read, tree edit and snapshot: concurrent edits
    // must each start from the head left by the preceding edit.
    let answer = state.changes.with_conn(|conn| {
        let id = changes::resolve(conn, &req.change)?;
        let current = changes::get(conn, id)?;
        if current.change.state.is_closed() {
            return Err(ChangeError::Refused(rosen_core::Error::ChangeClosed(id)));
        }
        let tree = rewrite(&store, current.change.head, &parts, blob)?;
        snapshots::record_on(
            conn,
            &store,
            &SnapshotRequest {
                change: id.to_string(),
                tree,
                from: Some(current.change.head),
                message: None,
            },
        )
    })?;
    if answer.recorded {
        snapshots::delivered(state);
    }
    Ok(answer)
}

/// Checks that a named blob is stored, whole, and is a blob: a tree entry
/// for a file must not name a tree or a commit, or a damaged object. The
/// check streams the body, so a large blob is never held whole.
fn stored_blob(store: &LooseObjectStore, id: ObjectId) -> Result<ObjectId, ChangeError> {
    match store.copy_raw_to(&id, &mut std::io::sink()) {
        Ok(Some((ObjectKind::Blob, _))) => Ok(id),
        Ok(Some((kind, _))) => Err(ChangeError::Refused(rosen_core::Error::InvalidPath(
            format!("{id} is a {kind:?}, not a blob"),
        ))),
        Ok(None) => Err(ChangeError::NotFound(format!(
            "the server does not hold blob {id}; upload it first"
        ))),
        Err(err) => Err(store_error(err)),
    }
}

fn validate_path(path: &str) -> Result<Vec<&str>, ChangeError> {
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() > 128
        || parts.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || p.eq_ignore_ascii_case(".git")
                || p.eq_ignore_ascii_case(".rosen")
                || p.contains(['\\', ':', '\0'])
        })
    {
        return Err(invalid(path));
    }
    Ok(parts)
}

fn rewrite(
    store: &dyn ObjectStore,
    root: ObjectId,
    parts: &[&str],
    blob: Option<ObjectId>,
) -> Result<ObjectId, ChangeError> {
    let empty = ObjectId::hash_object("tree", b"");
    let tree = if root == empty {
        Tree::default()
    } else {
        store.read_tree(&root).map_err(store_error)?
    };
    let name = parts[0].as_bytes();
    let old = tree.get(name);
    let replacement = if parts.len() == 1 {
        file_entry(old, name, blob)?
    } else {
        if old.is_some_and(|e| !e.mode.is_tree()) {
            return Err(invalid("parent is not a directory"));
        }
        if old.is_none() && blob.is_none() {
            return Ok(root);
        }
        let child = rewrite(store, old.map_or(empty, |e| e.id), &parts[1..], blob)?;
        (child != empty).then(|| TreeEntry {
            mode: FileMode::Directory,
            name: name.to_vec(),
            id: child,
        })
    };
    let mut entries: Vec<_> = tree
        .entries()
        .iter()
        .filter(|e| e.name != name)
        .cloned()
        .collect();
    entries.extend(replacement);
    store
        .put_tree(&Tree::new(entries).map_err(ChangeError::Refused)?)
        .map_err(store_error)
}

fn invalid(path: &str) -> ChangeError {
    ChangeError::Refused(rosen_core::Error::InvalidPath(path.to_owned()))
}

fn store_error(err: rosen_store::StoreError) -> ChangeError {
    ChangeError::Io(std::io::Error::other(err.to_string()))
}

/// The entry for a written file (`blob` already stored), or `None` for a
/// delete.
fn file_entry(
    old: Option<&TreeEntry>,
    name: &[u8],
    blob: Option<ObjectId>,
) -> Result<Option<TreeEntry>, ChangeError> {
    if old.is_some_and(|e| e.mode.is_tree() || e.mode == FileMode::Submodule) {
        return Err(invalid("path is not a file"));
    }
    Ok(blob.map(|id| TreeEntry {
        mode: old.map_or(FileMode::Regular, |e| e.mode),
        name: name.to_vec(),
        id,
    }))
}
