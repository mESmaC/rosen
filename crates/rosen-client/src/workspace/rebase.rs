//! Owner-requested application of a server tree at an agent step boundary.
use super::{layout::Layout, MaterialiseError, Sparse};
use crate::snapshot::{Snapshotter, WorkspaceMarker};
use crate::{Client, Error};
use rosen_core::{AgentId, ObjectId};
use rosen_protocol::{ChangeRecord, Request, Response};
use rosen_store::ObjectStore;
use std::path::Path;

impl Client {
    pub async fn rebase_change(&self, change: &str) -> Result<ChangeRecord, Error> {
        match self
            .request(Request::ChangeRebase {
                change: change.into(),
            })
            .await?
        {
            Response::Change(record) => Ok(record),
            other => Err(crate::connection::unexpected(other)),
        }
    }

    /// Call only after the owner stops its tools and watcher at a step boundary.
    /// The server never calls this method in response to a rebase event.
    pub async fn apply_rebase(&self, root: &Path, owner: &AgentId) -> Result<ObjectId, Error> {
        let marker = WorkspaceMarker::read(root)?;
        let change = self.change_show(&marker.change.to_string()).await?;
        if &change.change.owner != owner {
            return Err(Error::InvalidArgument(
                "only the owner can apply a rebase".into(),
            ));
        }
        self.ensure_local(marker.tree).await?;
        self.ensure_local(change.change.head).await?;
        let store = self.local_store().await?;
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut snapshotter = Snapshotter::open(&store, &root)?;
            snapshotter.apply_rebased_tree(&store, change.change.head)?;
            Ok(change.change.head)
        })
        .await
        .map_err(|err| Error::Unexpected(err.to_string()))?
    }
}

pub(crate) fn apply(
    store: &dyn ObjectStore,
    root: &Path,
    marker: &WorkspaceMarker,
    tree: ObjectId,
) -> Result<(), Error> {
    let sparse = Sparse::paths(&marker.sparse).map_err(Error::Workspace)?;
    let before = Layout::of_tree(store, marker.tree, &sparse).map_err(Error::Workspace)?;
    let after = Layout::of_tree(store, tree, &sparse).map_err(Error::Workspace)?;
    // Check ignored output too: it must never be overwritten by a new tracked file.
    for entry in &after.files {
        check_ancestors(root, &entry.path)?;
        let path = super::layout::disk_path(root, &entry.path);
        if !before.files.iter().any(|old| old.path == entry.path)
            && std::fs::symlink_metadata(&path).is_ok()
        {
            return Err(Error::InvalidArgument(format!(
                "rebase would overwrite {}",
                path.display()
            )));
        }
        store
            .read_blob(&entry.blob)
            .map_err(|err| Error::Workspace(MaterialiseError::Store(err)))?;
    }
    let changed = super::bring_to_tree(root, &before, &after).map_err(Error::Workspace)?;
    super::write_all(store, root, &changed).map_err(Error::Workspace)?;
    Ok(())
}

// An ignored directory can be a link outside the workspace. Never traverse
// it while bringing in newly tracked paths from the server.
fn check_ancestors(root: &Path, path: &[u8]) -> Result<(), Error> {
    if path
        .split(|b| *b == b'/')
        .next()
        .is_some_and(|p| p.eq_ignore_ascii_case(b".rosen"))
    {
        return Err(Error::InvalidArgument(
            "a rebase cannot write workspace metadata".into(),
        ));
    }
    let full = super::layout::disk_path(root, path);
    for dir in full.ancestors().skip(1).take_while(|p| *p != root) {
        if let Ok(meta) = std::fs::symlink_metadata(dir) {
            if meta.file_type().is_symlink() {
                return Err(Error::InvalidArgument(format!(
                    "rebase would traverse {}",
                    dir.display()
                )));
            }
        }
    }
    Ok(())
}
