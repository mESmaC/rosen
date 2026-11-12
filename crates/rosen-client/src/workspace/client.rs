//! Materialising a change's workspace through a connected client.

use std::path::{Path, PathBuf};

use rosen_core::ObjectId;
use rosen_protocol::ChangeRecord;
use rosen_store::LooseObjectStore;

use super::{materialise, Options, Report};
use crate::{Client, Error};

impl Client {
    /// The machine's local object store: the daemon's own store, which every
    /// workspace on this machine is materialised from (SPEC.md D8, R3).
    pub async fn local_store(&self) -> Result<LooseObjectStore, Error> {
        let status = self.status().await?;
        let objects = PathBuf::from(status.data_dir).join("objects");
        LooseObjectStore::open(objects).map_err(|error| Error::Workspace(error.into()))
    }

    /// Materialises `change`'s head tree into the new directory `dest`.
    pub async fn materialise_change(
        &self,
        change: &ChangeRecord,
        dest: &Path,
        options: Options,
    ) -> Result<Report, Error> {
        self.materialise_tree(change.change.head, dest, options)
            .await
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
        let store = self.local_store().await?;
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || materialise(&store, tree, &dest, &options))
            .await
            .map_err(|error| Error::Unexpected(format!("materialisation stopped: {error}")))?
            .map_err(Error::Workspace)
    }
}
