//! Direct edits of immutable trees, with no server checkout (SPEC R3).
use crate::changes::ChangeError;
use crate::{changes, snapshots, ServerState};
use rosen_core::{FileMode, ObjectId, Tree, TreeEntry};
use rosen_protocol::{ErrorBody, ErrorCode, Outcome, Response, SnapshotRequest, WorkspaceWrite};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

pub async fn write(state: &Arc<ServerState>, request: WorkspaceWrite) -> Outcome {
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

fn edit(
    state: &ServerState,
    req: WorkspaceWrite,
) -> Result<rosen_protocol::SnapshotOutcome, ChangeError> {
    let parts = validate_path(&req.path)?;
    let store = LooseObjectStore::open(state.config.objects_dir()).map_err(store_error)?;
    // Keep the connection through read, tree edit and snapshot: concurrent edits
    // must each start from the head left by the preceding edit.
    let answer = state.changes.with_conn(|conn| {
        let id = changes::resolve(conn, &req.change)?;
        let current = changes::get(conn, id)?;
        if current.change.state.is_closed() {
            return Err(ChangeError::Refused(rosen_core::Error::ChangeClosed(id)));
        }
        let tree = rewrite(
            &store,
            current.change.head,
            &parts,
            req.data.as_ref().map(|d| d.0.as_slice()),
        )?;
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
        snapshots::publish(state, &answer, false);
    }
    Ok(answer)
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
    data: Option<&[u8]>,
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
        file_entry(store, old, name, data)?
    } else {
        if old.is_some_and(|e| !e.mode.is_tree()) {
            return Err(invalid("parent is not a directory"));
        }
        if old.is_none() && data.is_none() {
            return Ok(root);
        }
        let child = rewrite(store, old.map_or(empty, |e| e.id), &parts[1..], data)?;
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

fn file_entry(
    store: &dyn ObjectStore,
    old: Option<&TreeEntry>,
    name: &[u8],
    data: Option<&[u8]>,
) -> Result<Option<TreeEntry>, ChangeError> {
    if old.is_some_and(|e| e.mode.is_tree() || e.mode == FileMode::Submodule) {
        return Err(invalid("path is not a file"));
    }
    data.map(|bytes| {
        Ok(TreeEntry {
            mode: old.map_or(FileMode::Regular, |e| e.mode),
            name: name.to_vec(),
            id: store.put_blob(bytes).map_err(store_error)?,
        })
    })
    .transpose()
}
