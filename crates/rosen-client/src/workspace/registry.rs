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
                records.push(
                    serde_json::from_slice(&std::fs::read(path)?).map_err(std::io::Error::other)?,
                );
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
        let record = WorkspaceRecord { change, path };
        let bytes = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
        let file = record_file(&dir, &record)?;
        let temp = file.with_extension(format!("{}.tmp", std::process::id()));
        let mut output = std::fs::File::create(&temp)?;
        output.write_all(&bytes)?;
        output.sync_all()?;
        drop(output);
        std::fs::rename(temp, file)?;
        Ok(())
    }

    pub(super) async fn unregister_workspace(&self, record: &WorkspaceRecord) -> Result<(), Error> {
        let file = record_file(&self.workspace_registry().await?, record)?;
        match std::fs::remove_file(file) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    async fn workspace_registry(&self) -> Result<PathBuf, Error> {
        Ok(PathBuf::from(self.status().await?.data_dir).join("workspaces"))
    }
}

fn record_file(dir: &Path, record: &WorkspaceRecord) -> Result<PathBuf, Error> {
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let id = rosen_core::ObjectId::hash_object("blob", &bytes);
    Ok(dir.join(format!("{id}.json")))
}
