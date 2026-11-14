//! Materialising a change's workspace through a connected client.

use std::path::{Path, PathBuf};

use rosen_core::ObjectId;
use rosen_protocol::{
    ChangeRecord, EnsureObjects, LandedProseQuery, PooledWorkspace, Request, Response,
};
use rosen_store::LooseObjectStore;

use super::{
    limits::{Budget, Load, Settings},
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
        self.ensure_sparse_local(root, Vec::new()).await
    }

    /// Like [`Client::ensure_local`] for a sparse workspace holding `sparse`
    /// (normalised, as [`super::Sparse::chosen`] gives them): every tree, but
    /// only the blobs at or under those paths. Empty means the whole tree.
    pub async fn ensure_sparse_local(
        &self,
        root: ObjectId,
        sparse: Vec<String>,
    ) -> Result<u64, Error> {
        let request = Request::EnsureObjects(EnsureObjects { root, sparse });
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
            // A daemon may bring a ready workspace from another tree to the
            // head, which needs the head's objects in its store (D8).
            self.ensure_local(change.change.head).await?;
            self.take_workspace(change.change.head, dest).await?
        } else {
            Err(None)
        };
        // A ready workspace's stats were recorded before the take.
        let seed_stats = pooled.is_err();
        let report = match pooled {
            Ok(taken) => Report {
                method: if taken.from_template {
                    super::Method::BlockClone
                } else {
                    super::Method::WarmPool
                },
                files: taken.files,
                cloned: 0,
                copied: 0,
                written: 0,
                bytes_written: 0,
                fallback: None,
                pool_missed: None,
                prepared: taken.prepared,
            },
            Err(missed) => Report {
                pool_missed: missed,
                ..self
                    .materialise_tree(change.change.head, dest, options)
                    .await?
            },
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
            if seed_stats {
                // Only a speed-up for the first snapshot (#174).
                let _ = super::seed_stats(&store, &root, head);
            }
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
        // its cache lacks (D8): for a sparse workspace, only its blobs (R3).
        self.ensure_sparse_local(tree, options.sparse.chosen())
            .await?;
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

    /// Sets either of this machine's materialisation admission caps, when
    /// given, and returns the caps in force with the load against them
    /// (SPEC R3). The caps persist under the daemon's data directory, so
    /// they hold for every client and pool worker until changed again.
    pub async fn load_limits(
        &self,
        materialisations: Option<usize>,
        disk_bytes: Option<u64>,
    ) -> Result<Load, Error> {
        let limits_root = PathBuf::from(self.status().await?.data_dir).join("workspace-limits");
        let path = limits_root.clone();
        tokio::task::spawn_blocking(move || {
            let budget = Budget::open(&limits_root, Settings::default())?;
            budget.set_caps(materialisations, disk_bytes)?;
            budget.load()
        })
        .await
        .map_err(|error| Error::Unexpected(format!("load limits stopped: {error}")))?
        .map_err(|error| Error::Workspace(super::MaterialiseError::io(&path, error)))
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

    /// Takes a ready workspace holding `tree`, prepared ones first, or a
    /// clone of the daemon's template. When the daemon has neither, the
    /// inner error says why if the pool can never serve this destination.
    async fn take_workspace(
        &self,
        tree: ObjectId,
        dest: &Path,
    ) -> Result<Result<PooledWorkspace, Option<String>>, Error> {
        let dest = std::path::absolute(dest)?.to_string_lossy().into_owned();
        match self.request(Request::PoolTake { tree, dest }).await? {
            Response::PoolTaken(taken) => Ok(taken.ok_or(None)),
            Response::PoolMissed(why) => Ok(Err(Some(why))),
            other => Err(unexpected(other)),
        }
    }

    /// Whether every land after base commit `since` through `through` was
    /// prose-only, so a workspace prepared at `since` is still prepared.
    pub async fn landed_prose(&self, since: ObjectId, through: ObjectId) -> Result<bool, Error> {
        let query = LandedProseQuery { since, through };
        match self.request(Request::LandedProse(query)).await? {
            Response::LandedProse(prose) => Ok(prose),
            other => Err(unexpected(other)),
        }
    }
}
