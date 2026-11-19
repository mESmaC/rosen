//! Workspace lifecycle and final snapshot guard (SPEC.md R4).
use super::delete::path_summary;
use super::registry::WorkspaceRecord;
use crate::snapshot::{Snapshotter, WorkspaceMarker};
use crate::{Client, Error};
use rosen_core::{ChangeId, ChangeState};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

const CLEANUP_RETRY_MIN: Duration = Duration::from_secs(1);
const CLEANUP_RETRY_MAX: Duration = Duration::from_secs(60);

/// A refused cleanup is reported per workspace, so one busy owner cannot
/// stop cleanup of unrelated landed changes.
#[derive(Debug)]
pub struct CleanupReport {
    pub path: PathBuf,
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct CleanupRetries {
    pending: HashMap<(ChangeId, PathBuf), CleanupRetry>,
}

impl CleanupRetries {
    pub(super) fn next_attempt(&self) -> Option<Instant> {
        self.pending.values().map(|retry| retry.at).min()
    }

    fn is_deferred(&self, key: &(ChangeId, PathBuf), now: Instant) -> bool {
        self.pending.get(key).is_some_and(|retry| retry.at > now)
    }
}

struct CleanupRetry {
    at: Instant,
    delay: Duration,
}

impl CleanupRetry {
    fn after_refusal(previous: Option<Self>, now: Instant) -> Self {
        let delay = previous.map_or(CLEANUP_RETRY_MIN, |retry| {
            (retry.delay * 2).min(CLEANUP_RETRY_MAX)
        });
        Self {
            at: now + delay,
            delay,
        }
    }
}

impl Client {
    /// Flushes writes while the change is open, then independently rehashes
    /// disk contents. Any inability to establish durability keeps the files.
    pub async fn workspace_delete(&self, root: &Path) -> Result<(), Error> {
        let _cleanup = self.cleanup_lock().await?;
        self.delete_locked(root).await
    }

    /// [`Client::workspace_delete`] for a caller holding the cleanup lock.
    async fn delete_locked(&self, root: &Path) -> Result<(), Error> {
        reject_symlink(root)?;
        let root = std::fs::canonicalize(root)?;
        self.check_workspace_boundary(&root).await?;
        let marker = WorkspaceMarker::read(&root)?;
        let record = WorkspaceRecord {
            change: marker.change,
            path: root.clone(),
            kept: None,
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
        let original_lease = self.verify_workspace_durable(&root, false).await?;
        // The original registration also identifies this deterministic
        // sibling, so a crash after rename can be recovered at daemon start.
        std::fs::rename(&root, &staged)?;
        let staged_result = self.verify_workspace_durable(&staged, false).await;
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
        // Both directories are gone. Their lease files go while still held
        // exclusive, so no writer is left locking a removed file.
        staged_lease.close_deleted();
        original_lease.close_deleted();
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

    /// `staged` marks a directory renamed aside only after an earlier pass
    /// verified it. Removal there may have been interrupted after deleting
    /// some verified files, so a missing tracked file is already removed
    /// rather than an unstored deletion; changed and added files still keep
    /// it (issue #90).
    async fn verify_workspace_durable(
        &self,
        root: &Path,
        staged: bool,
    ) -> Result<Snapshotter, Error> {
        let marker = WorkspaceMarker::read(root)?;
        self.ensure_sparse_local(marker.tree, marker.sparse.clone())
            .await?;
        let store = self.local_store().await?;
        let path = root.to_path_buf();
        // Opening waits for any snapshot writer, such as another process's
        // upload, so it must not hold a runtime worker meanwhile (#93).
        let scan = tokio::task::spawn_blocking(move || hold_if_durable(&store, &path, staged))
            .await
            .map_err(|err| Error::Unexpected(format!("workspace check stopped: {err}")))??;
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

    /// Reconciles durable registrations after startup or a change event:
    /// deletes the workspaces of landed and abandoned changes (R4). A refusal
    /// is kept on the registration, so `rosen workspace list` and
    /// `Session::workspaces` report why the workspace is still there.
    pub async fn cleanup_workspaces(&self) -> Result<Vec<CleanupReport>, Error> {
        self.cleanup_workspaces_with(&mut CleanupRetries::default(), None)
            .await
    }

    pub(crate) async fn cleanup_change_workspaces(
        &self,
        change: ChangeId,
    ) -> Result<Vec<CleanupReport>, Error> {
        self.cleanup_workspaces_with(&mut CleanupRetries::default(), Some(change))
            .await
    }

    pub(super) async fn cleanup_workspaces_with(
        &self,
        retries: &mut CleanupRetries,
        only_change: Option<ChangeId>,
    ) -> Result<Vec<CleanupReport>, Error> {
        let _cleanup = self.cleanup_lock().await?;
        let records = self.workspace_list().await?;
        let mut previous = std::mem::take(retries);
        let mut reports = Vec::new();
        for record in records {
            let key = (record.change, record.path.clone());
            if only_change.is_some_and(|change| change != record.change)
                || previous.is_deferred(&key, Instant::now())
            {
                retries.pending.extend(previous.pending.remove_entry(&key));
                continue;
            }
            let result = self.cleanup_workspace(&record).await;
            if result.is_err() {
                // Schedule from the end of the attempt. Hashing a large
                // workspace can itself take longer than the retry interval.
                let retry =
                    CleanupRetry::after_refusal(previous.pending.remove(&key), Instant::now());
                retries.pending.insert(key, retry);
            }
            let kept = match &result {
                Ok(_) => None,
                Err(err) => Some(err.to_string()),
            };
            // A finished cleanup already removed the registration. A lost
            // server connection says nothing about the workspace, so it
            // neither records nor clears a reason.
            let unreachable = matches!(result, Err(Error::Disconnected | Error::NotRunning(_)));
            if !matches!(result, Ok(true)) && !unreachable {
                if let Err(err) = self.set_kept_reason(&record, kept.clone()).await {
                    eprintln!(
                        "rosen workspace cleanup {}: recording reason: {err}",
                        record.path.display()
                    );
                }
            }
            if !matches!(result, Ok(false)) {
                reports.push(CleanupReport {
                    path: record.path,
                    error: kept,
                });
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
            let mut staged_lease = self.verify_workspace_durable(&staged, true).await?;
            staged_lease.remove_verified()?;
            staged_lease.close_deleted();
            self.unregister_workspace(record).await?;
        } else {
            verify_registration(&record.path, record)?;
            self.delete_locked(&record.path).await?;
        }
        Ok(true)
    }
}

fn staging_path(record: &WorkspaceRecord) -> Result<PathBuf, Error> {
    let parent = record
        .path
        .parent()
        .ok_or_else(|| kept("cannot delete a root"))?;
    let id = record.identity()?;
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

/// Opens the workspace, holds it for deletion and checks that every file is
/// stored on the server. Blocking.
fn hold_if_durable(
    store: &dyn rosen_store::ObjectStore,
    root: &Path,
    staged: bool,
) -> Result<Snapshotter, Error> {
    let mut scan = Snapshotter::open(store, root)?;
    scan.hold_for_deletion()?;
    let journal = crate::snapshot::pending_journal(root)?;
    if !deletion_permitted(0, journal, 0) {
        return Err(kept(path_summary(
            "unuploaded files",
            &scan.unuploaded_paths()?,
        )));
    }
    let paths = if staged {
        scan.unstored_paths_on_disk()?
    } else {
        scan.unstored_paths()?
    };
    if !deletion_permitted(0, 0, paths.len()) {
        return Err(kept(path_summary("unstored files", &paths)));
    }
    Ok(scan)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_only_defers_that_workspace() {
        // R4: a busy workspace must neither be rehashed on every unrelated
        // change event nor delay cleanup of a newly closed workspace.
        let now = Instant::now();
        let change = ChangeId::from_bytes([1; 16]);
        let refused = (change, PathBuf::from("refused"));
        let newly_closed = (change, PathBuf::from("newly-closed"));
        let mut retries = CleanupRetries::default();
        retries
            .pending
            .insert(refused.clone(), CleanupRetry::after_refusal(None, now));
        assert!(retries.is_deferred(&refused, now));
        assert!(!retries.is_deferred(&newly_closed, now));
        let first = now + Duration::from_secs(1);
        assert_eq!(retries.next_attempt(), Some(first));
        assert!(!retries.is_deferred(&refused, first));
        let second = CleanupRetry::after_refusal(retries.pending.remove(&refused), first);
        assert_eq!(second.at, first + Duration::from_secs(2));
    }
}
