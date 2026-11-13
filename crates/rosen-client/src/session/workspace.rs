//! Workspace creation, snapshots and deletion through a session (SPEC.md
//! R2, R3, R4). Velocity calls these in place of `git worktree`.

use super::{Session, WorkspaceWatch};
use crate::snapshot::SnapshotReport;
use crate::workspace::{Options, Report, Sparse, WorkspaceRecord};
use crate::Error;
use rosen_protocol::ChangeRecord;
use std::path::{Path, PathBuf};

/// Where and what [`Session::create_workspace`] makes.
#[derive(Debug, Clone)]
pub struct WorkspaceSpec {
    /// The new workspace's directory. It must not exist yet.
    pub dir: PathBuf,
    /// A change ID or unique prefix to stack on; `None` starts at the base.
    pub on: Option<String>,
    /// Which paths to hold. A sparse workspace is never taken from the warm
    /// pool, which holds only whole trees.
    pub sparse: Sparse,
}

impl WorkspaceSpec {
    /// A whole workspace at `dir` on the current base.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            on: None,
            sparse: Sparse::all(),
        }
    }

    pub fn on(mut self, change: impl Into<String>) -> Self {
        self.on = Some(change.into());
        self
    }

    pub fn sparse(mut self, sparse: Sparse) -> Self {
        self.sparse = sparse;
        self
    }
}

/// A new change and its workspace on disk.
#[derive(Debug, Clone)]
pub struct CreatedWorkspace {
    pub change: ChangeRecord,
    /// The workspace's root, as given in the spec.
    pub root: PathBuf,
    /// How its files were made: a warm-pool take, a block clone or a copy.
    pub report: Report,
}

impl Session {
    /// Creates a change owned by this agent and materialises its workspace,
    /// taking a ready one from the warm pool when it can (R3).
    ///
    /// The change is created first. If the workspace then fails, the error is
    /// [`Error::WorkspaceNotCreated`] naming the change, which still exists:
    /// retry with [`Session::open_workspace`] or abandon it.
    pub async fn create_workspace(&self, spec: WorkspaceSpec) -> Result<CreatedWorkspace, Error> {
        let change = self.new_change(spec.on.as_deref()).await?;
        let options = Options {
            sparse: spec.sparse,
            source: None,
        };
        match self
            .client
            .materialise_change(&change, &spec.dir, options)
            .await
        {
            Ok(report) => Ok(CreatedWorkspace {
                change,
                root: spec.dir,
                report,
            }),
            Err(source) => Err(Error::WorkspaceNotCreated {
                change: change.change.id,
                source: Box::new(source),
            }),
        }
    }

    /// Materialises another workspace for an existing change at its current
    /// head, for example after a failed [`Session::create_workspace`] or on
    /// a second machine. `dir` must not exist yet.
    pub async fn open_workspace(
        &self,
        change: &str,
        dir: &Path,
        sparse: Sparse,
    ) -> Result<Report, Error> {
        let record = self.client.change_show(change).await?;
        let options = Options {
            sparse,
            source: None,
        };
        self.client.materialise_change(&record, dir, options).await
    }

    /// Every workspace registered on this machine, whoever owns its change.
    pub async fn workspaces(&self) -> Result<Vec<WorkspaceRecord>, Error> {
        self.client.workspace_list().await
    }

    /// Stores a snapshot of the whole workspace now: what `rosen commit`
    /// does. With a message, one is recorded even when nothing changed.
    /// Refused while a [`WorkspaceWatch`] holds the workspace; stop it first.
    pub async fn snapshot(
        &self,
        root: &Path,
        message: Option<String>,
    ) -> Result<SnapshotReport, Error> {
        self.client.snapshot_workspace(root, message).await
    }

    /// Starts write-through snapshots of the workspace in the background
    /// (R2). The watcher keeps working through daemon restarts and server
    /// outages, journalling offline; see [`WorkspaceWatch`].
    pub fn watch(&self, root: &Path) -> WorkspaceWatch {
        WorkspaceWatch::start(self.endpoint.clone(), root.to_path_buf())
    }

    /// Deletes the workspace under the deletion guard (R4): it flushes a
    /// final snapshot, and refuses, naming the files and keeping the
    /// workspace, while any edit is not stored on the server.
    pub async fn delete_workspace(&self, root: &Path) -> Result<(), Error> {
        self.client.workspace_delete(root).await
    }
}
