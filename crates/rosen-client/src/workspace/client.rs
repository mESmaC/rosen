//! Materialising a change's workspace through a connected client.

use std::path::{Path, PathBuf};

use rosen_core::ObjectId;
use rosen_protocol::{ChangeRecord, EnsureObjects, Request, Response};
use rosen_store::LooseObjectStore;

use super::{
    limits::{Budget, Settings},
    materialise_with_budget, Options, Report,
};
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
        let pooled = if options.sparse.is_all() && options.source.is_none() {
            self.take_workspace(change.change.head, dest).await?
        } else {
            None
        };
        let report = match pooled {
            Some(files) => Report {
                method: super::Method::WarmPool,
                files,
                cloned: 0,
                copied: 0,
                written: 0,
                bytes_written: 0,
                fallback: None,
            },
            None => {
                self.materialise_tree(change.change.head, dest, options)
                    .await?
            }
        };
        // The base's ignore list, never the change's own `rosen.toml` (R2).
        let ignore = self.base_config().await?.ignore;
        WorkspaceMarker::new(change.change.id, change.change.head, sparse, ignore)
            .write(dest)
            .map_err(Error::Io)?;
        // Seed tree metadata so a watcher can start during a server outage.
        // A warm-pool workspace was seeded before the take, so this finds
        // the head tree cached and returns at once.
        let store = self.local_store().await?;
        let (root, head) = (dest.to_path_buf(), change.change.head);
        tokio::task::spawn_blocking(move || {
            crate::snapshot::seed_tree_cache(&store, &root, head)
        })
        .await
        .map_err(|error| Error::Unexpected(format!("tree cache seeding stopped: {error}")))??;
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
        let limits_root = PathBuf::from(self.status().await?.data_dir).join("workspace-limits");
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let budget = Budget::open(&limits_root, Settings::default())
                .map_err(|error| super::MaterialiseError::io(&limits_root, error))?;
            materialise_with_budget(&store, tree, &dest, &options, Some(&budget))
        })
        .await
        .map_err(|error| Error::Unexpected(format!("materialisation stopped: {error}")))?
        .map_err(Error::Workspace)
    }
}

impl Client {
    /// Changes this machine's ready-pool target. Zero disables the pool.
    pub async fn set_pool_size(&self, size: usize) -> Result<(), Error> {
        match self.request(Request::PoolSize { size }).await? {
            Response::PoolSized => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    async fn take_workspace(&self, tree: ObjectId, dest: &Path) -> Result<Option<u64>, Error> {
        let dest = std::path::absolute(dest)?.to_string_lossy().into_owned();
        match self.request(Request::PoolTake { tree, dest }).await? {
            Response::PoolTaken(files) => Ok(files),
            other => Err(unexpected(other)),
        }
    }
}
