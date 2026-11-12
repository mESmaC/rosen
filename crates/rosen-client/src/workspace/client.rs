//! Materialising a change's workspace through a connected client.

use std::path::{Path, PathBuf};

use rosen_core::ObjectId;
use rosen_protocol::{ChangeRecord, EnsureObjects, Request, Response};
use rosen_store::LooseObjectStore;

use super::{materialise, Options, Report};
use crate::connection::unexpected;
use crate::snapshot::WorkspaceMarker;
use crate::{Client, Error};

impl Client {
    /// The machine's local object store: the daemon's own store, which every
    /// workspace on this machine is materialised from (SPEC.md D8, R3).
    pub async fn local_store(&self) -> Result<LooseObjectStore, Error> {
        let status = self.status().await?;
        let objects = PathBuf::from(status.data_dir).join("objects");
        LooseObjectStore::open(objects).map_err(|error| Error::Workspace(error.into()))
    }

    /// Makes every object reachable from `root` readable from the local
    /// store. On the server's machine this only checks the root is there.
    /// Returns how many objects the daemon fetched.
    pub async fn ensure_local(&self, root: ObjectId) -> Result<u64, Error> {
        let request = Request::EnsureObjects(EnsureObjects { root });
        match self.request(request).await? {
            Response::ObjectsLocal(local) => Ok(local.fetched),
            other => Err(unexpected(other)),
        }
    }

    /// Materialises `change`'s head tree into the new directory `dest`, and
    /// marks it as the change's workspace so its writes become snapshots.
    pub async fn materialise_change(
        &self,
        change: &ChangeRecord,
        dest: &Path,
        options: Options,
    ) -> Result<Report, Error> {
        let sparse = options.sparse.chosen();
        let report = self
            .materialise_tree(change.change.head, dest, options)
            .await?;
        WorkspaceMarker::new(change.change.id, change.change.head, sparse)
            .write(dest)
            .map_err(Error::Io)?;
        self.register_workspace(change.change.id, dest).await?;
        Ok(report)
    }

    /// Materialises any tree in the local store into the new directory
    /// `dest`. The copy runs on a blocking thread, so the connection keeps
    /// serving other requests and events meanwhile.
    pub async fn materialise_tree(
        &self,
        tree: ObjectId,
        dest: &Path,
        options: Options,
    ) -> Result<Report, Error> {
        // On a machine that is not the server, the daemon first fetches what
        // its cache lacks (D8).
        self.ensure_local(tree).await?;
        let store = self.local_store().await?;
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || materialise(&store, tree, &dest, &options))
            .await
            .map_err(|error| Error::Unexpected(format!("materialisation stopped: {error}")))?
            .map_err(Error::Workspace)
    }
}
