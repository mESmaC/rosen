//! Workspace lifecycle and final snapshot guard (SPEC.md R4).
use super::registry::WorkspaceRecord;
use crate::snapshot::{Snapshotter, WorkspaceMarker};
use crate::{Client, Error};
use rosen_core::ChangeState;
use std::path::{Path, PathBuf};

/// A refused cleanup is reported per workspace, so one busy owner cannot
/// stop cleanup of unrelated landed changes.
#[derive(Debug)]
pub struct CleanupReport {
    pub path: PathBuf,
    pub error: Option<String>,
}

impl Client {
    /// Flushes writes while the change is open, then independently rehashes
    /// disk contents. Any inability to establish durability keeps the files.
    pub async fn workspace_delete(&self, root: &Path) -> Result<(), Error> {
        reject_symlink(root)?;
        let root = std::fs::canonicalize(root)?;
        self.check_workspace_boundary(&root).await?;
        let marker = WorkspaceMarker::read(&root)?;
        let record = WorkspaceRecord {
            change: marker.change,
            path: root.clone(),
        };
        let staged = staging_path(&record)?;
        if staged.try_exists()? {
            return Err(kept(format!(
                "deletion staging path already exists: {}",
                staged.display()
            )));
        }
        let change = self.change_show(&marker.change.to_string()).await?;
        if !change.change.state.is_closed() {
            self.snapshot_workspace(&root, None).await?;
        }
        let _original_lease = self.verify_workspace_durable(&root).await?;
        // The original registration also identifies this deterministic
        // sibling, so a crash after rename can be recovered at daemon start.
        std::fs::rename(&root, &staged)?;
        let staged_result = self.verify_workspace_durable(&staged).await;
        if let Err(err) = &staged_result {
            // Never replace a directory someone created at the old path.
            if !root.try_exists()? {
                let _ = std::fs::rename(&staged, &root);
            }
            return Err(kept(format!(
                "{err}; checked directory: {}",
                staged.display()
            )));
        }
        let mut staged_lease = staged_result?;
        staged_lease.remove_verified()?;
        self.unregister_workspace(&record).await?;
        Ok(())
    }

    async fn check_workspace_boundary(&self, root: &Path) -> Result<(), Error> {
        for record in self.workspace_list().await? {
            if record.path != root && record.path.starts_with(root) {
                return Err(kept(format!("contains {}", record.path.display())));
            }
        }
        if root
            .ancestors()
            .skip(1)
            .any(|parent| WorkspaceMarker::path(parent).exists())
        {
            return Err(kept("nested workspace"));
        }
        Ok(())
    }

    async fn verify_workspace_durable(&self, root: &Path) -> Result<Snapshotter, Error> {
        let marker = WorkspaceMarker::read(root)?;
        self.ensure_local(marker.tree).await?;
        let store = self.local_store().await?;
        let mut scan = Snapshotter::open(&store, root)?;
        scan.hold_for_deletion()?;
        let journal = crate::snapshot::pending_journal(root)?;
        if !deletion_permitted(0, journal, 0) {
            return Err(kept(format!(
                "unuploaded files: {}",
                scan.unuploaded_paths()?.join(", ")
            )));
        }
        let paths = scan.unstored_paths()?;
        if !deletion_permitted(0, 0, paths.len()) {
            return Err(kept(format!("unstored files: {}", paths.join(", "))));
        }
        // The marker is only a hint: verify that this tree really belongs
        // to a snapshot stored by this server before authorising removal.
        let change = self.change_show(&marker.change.to_string()).await?;
        let stored = change.change.head == marker.tree
            || change
                .change
                .snapshots
                .iter()
                .any(|s| s.tree == marker.tree);
        if !stored {
            return Err(kept("marker tree is not stored for this change"));
        }
        Ok(scan)
    }

    /// Reconciles durable registrations after startup or a change event.
    pub async fn cleanup_workspaces(&self) -> Result<Vec<CleanupReport>, Error> {
        let mut reports = Vec::new();
        for record in self.workspace_list().await? {
            let result = self.cleanup_workspace(&record).await;
            match result {
                Ok(false) => {}
                result => reports.push(CleanupReport {
                    path: record.path,
                    error: result.err().map(|err| err.to_string()),
                }),
            }
        }
        Ok(reports)
    }

    async fn cleanup_workspace(&self, record: &WorkspaceRecord) -> Result<bool, Error> {
        let staged = staging_path(record)?;
        let original_exists = record.path.try_exists()?;
        let staged_exists = staged.try_exists()?;
        if !original_exists && !staged_exists {
            self.unregister_workspace(record).await?;
            return Ok(false);
        }
        let change = self.change_show(&record.change.to_string()).await?;
        if !matches!(
            change.change.state,
            ChangeState::Landed | ChangeState::Abandoned
        ) {
            return Ok(false);
        }
        if staged_exists {
            if original_exists {
                return Err(kept(format!(
                    "both original and staged workspace exist: {}",
                    staged.display()
                )));
            }
            verify_registration(&staged, record)?;
            self.check_workspace_boundary(&record.path).await?;
            self.check_workspace_boundary(&staged).await?;
            let mut staged_lease = self.verify_workspace_durable(&staged).await?;
            staged_lease.remove_verified()?;
            self.unregister_workspace(record).await?;
        } else {
            verify_registration(&record.path, record)?;
            self.workspace_delete(&record.path).await?;
        }
        Ok(true)
    }
}

fn staging_path(record: &WorkspaceRecord) -> Result<PathBuf, Error> {
    let parent = record
        .path
        .parent()
        .ok_or_else(|| kept("cannot delete a root"))?;
    let body = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let id = rosen_core::ObjectId::hash_object("blob", &body);
    Ok(parent.join(format!(".rosen-delete-{}-{id}", record.change)))
}

fn verify_registration(root: &Path, record: &WorkspaceRecord) -> Result<(), Error> {
    reject_symlink(root)?;
    if WorkspaceMarker::read(root)?.change != record.change {
        return Err(kept(format!(
            "registered change does not own {}",
            root.display()
        )));
    }
    Ok(())
}

fn reject_symlink(root: &Path) -> Result<(), Error> {
    if std::fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(kept("root is a symlink"));
    }
    Ok(())
}

fn kept(reason: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("workspace kept; {reason}"))
}

/// Whether the three independent sources of unstored work are all empty.
/// The caller obtains these observations while holding the workspace lease.
pub fn deletion_permitted(
    unsnapshotted_writes: usize,
    pending_journal: usize,
    hash_mismatches: usize,
) -> bool {
    unsnapshotted_writes == 0 && pending_journal == 0 && hash_mismatches == 0
}
