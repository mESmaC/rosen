//! Durable per-workspace registrations in the machine daemon's data directory.
use crate::{Client, Error};
use rosen_core::ChangeId;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub change: ChangeId,
    pub path: PathBuf,
    /// Why the last automatic cleanup kept this workspace (R4), until a
    /// later cleanup succeeds or the change reopens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
}

/// The fields that identify a registration. Its hash names the record file
/// and the deletion staging directory, so it must not include `kept`: a
/// staged directory from before a refusal is still found after one.
#[derive(Serialize)]
struct RecordKey<'a> {
    change: &'a ChangeId,
    path: &'a Path,
}

impl WorkspaceRecord {
    /// A stable content hash of the registration's identity.
    pub(super) fn identity(&self) -> Result<rosen_core::ObjectId, Error> {
        let key = RecordKey {
            change: &self.change,
            path: &self.path,
        };
        let bytes = serde_json::to_vec(&key).map_err(std::io::Error::other)?;
        Ok(rosen_core::ObjectId::hash_object("blob", &bytes))
    }
}

impl Client {
    pub async fn workspace_list(&self) -> Result<Vec<WorkspaceRecord>, Error> {
        let dir = self.workspace_registry().await?;
        let mut records = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(records),
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                if let Some(bytes) = read_listed(&path).await? {
                    records.push(serde_json::from_slice(&bytes).map_err(std::io::Error::other)?);
                }
            }
        }
        Ok(records)
    }

    pub(crate) async fn register_workspace(
        &self,
        change: ChangeId,
        root: &Path,
    ) -> Result<(), Error> {
        let path = std::fs::canonicalize(root)?;
        let dir = self.workspace_registry().await?;
        std::fs::create_dir_all(&dir)?;
        let record = WorkspaceRecord {
            change,
            path,
            kept: None,
        };
        write_record(&dir, &record)
    }

    /// Records why automatic cleanup kept the workspace, or clears it with
    /// `None`. Only rewrites when the reason changed, so a cleanup retried
    /// every second does not rewrite the registration every second.
    pub(super) async fn set_kept_reason(
        &self,
        record: &WorkspaceRecord,
        kept: Option<String>,
    ) -> Result<(), Error> {
        if record.kept == kept {
            return Ok(());
        }
        let dir = self.workspace_registry().await?;
        // Do not recreate a registration a finished cleanup removed. A
        // removal racing this check is repaired by the next cleanup pass,
        // which drops a registration whose directories are both gone.
        if !record_file(&dir, record)?.try_exists()? {
            return Ok(());
        }
        let updated = WorkspaceRecord {
            kept,
            ..record.clone()
        };
        write_record(&dir, &updated)
    }

    pub(super) async fn unregister_workspace(&self, record: &WorkspaceRecord) -> Result<(), Error> {
        let file = record_file(&self.workspace_registry().await?, record)?;
        match std::fs::remove_file(file) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Serializes workspace deletion on this machine. The daemon cleans up
    /// on the same events that an abandoning client cleans up after, and two
    /// passes on one workspace would make the loser report the winner's
    /// lease or removal as a refusal. The OS releases the lock on a crash.
    pub(super) async fn cleanup_lock(&self) -> Result<std::fs::File, Error> {
        let dir = self.workspace_registry().await?;
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("cleanup.lock"))?;
        let locked = tokio::task::spawn_blocking(move || file.lock().map(|()| file))
            .await
            .map_err(|error| Error::Unexpected(error.to_string()))?;
        Ok(locked?)
    }

    async fn workspace_registry(&self) -> Result<PathBuf, Error> {
        Ok(PathBuf::from(self.status().await?.data_dir).join("workspaces"))
    }
}

/// Tries of a record read that Windows denies while the file is being
/// deleted or replaced, and the pause between them.
const DENIED_TRIES: u32 = 20;
const DENIED_PAUSE: std::time::Duration = std::time::Duration::from_millis(5);

/// Reads a record the listing found, or `None` when another process
/// unregistered it after the directory was read: a cleanup pass or another
/// client's abandon must not fail this one's listing (issue #170). Windows
/// denies access to a file for the moment it is deleted or replaced, so a
/// denial is retried briefly; one that persists is a real permission error.
async fn read_listed(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    let mut tries = 0;
    loop {
        match std::fs::read(path) {
            Ok(bytes) => return Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                tries += 1;
                if tries >= DENIED_TRIES {
                    return Err(err.into());
                }
                tokio::time::sleep(DENIED_PAUSE).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn record_file(dir: &Path, record: &WorkspaceRecord) -> Result<PathBuf, Error> {
    Ok(dir.join(format!("{}.json", record.identity()?)))
}

/// Replaces the record file atomically, so a crash leaves the old or the
/// new registration and never a torn one.
fn write_record(dir: &Path, record: &WorkspaceRecord) -> Result<(), Error> {
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let file = record_file(dir, record)?;
    let temp = file.with_extension(format!("{}.tmp", std::process::id()));
    let mut output = std::fs::File::create(&temp)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    drop(output);
    std::fs::rename(temp, file)?;
    Ok(())
}
