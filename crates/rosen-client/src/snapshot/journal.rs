//! Durable, immutable snapshot records and separate upload acknowledgements.
use super::{Prepared, SnapshotError, STATE_DIR};
use rosen_core::ObjectKind;
use rosen_store::{LooseObjectStore, ObjectStore};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
pub(super) struct Record {
    pub prepared: Prepared,
    pub message: Option<String>,
}

pub(super) struct Journal {
    dir: PathBuf,
    // OS lock releases on process death; stale lock files never block recovery.
    _lock: File,
}

impl Journal {
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join(STATE_DIR).join("journal");
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("lock"))?;
        lock.try_lock().map_err(io::Error::other)?;
        Ok(Self { dir, _lock: lock })
    }

    pub fn records(&self) -> io::Result<Vec<(u64, Record)>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|x| x.to_str())
                .and_then(|x| x.parse::<u64>().ok())
                .ok_or_else(|| io::Error::other("invalid journal sequence"))?;
            let record = serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
            records.push((id, record));
        }
        records.sort_by_key(|(id, _)| *id);
        Ok(records)
    }

    pub fn acknowledged(&self, id: u64) -> bool {
        self.dir.join(format!("{id}.ack")).exists()
    }

    pub fn append(&self, record: &Record) -> io::Result<()> {
        let next = self.records()?.last().map_or(1, |(id, _)| id + 1);
        self.publish(
            &format!("{next}.json"),
            &serde_json::to_vec(record).map_err(io::Error::other)?,
        )
    }

    pub fn acknowledge(&self, id: u64) -> io::Result<()> {
        if self.acknowledged(id) {
            return Ok(());
        }
        self.publish(&format!("{id}.ack"), b"stored")
    }

    fn publish(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let temp = self.dir.join("pending.tmp");
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(temp, self.dir.join(name))
    }
}

pub(super) fn cache(root: &Path) -> Result<LooseObjectStore, SnapshotError> {
    Ok(LooseObjectStore::open(
        root.join(STATE_DIR).join("snapshot-trees"),
    )?)
}

pub(super) fn save_trees(root: &Path, prepared: &Prepared) -> Result<(), SnapshotError> {
    let store = cache(root)?;
    for (_, body) in &prepared.trees {
        store.put_raw(ObjectKind::Tree, body)?;
    }
    Ok(())
}

/// Number of snapshots still requiring server acknowledgement (SPEC R4).
pub fn pending_journal(root: &Path) -> io::Result<usize> {
    let dir = root.join(STATE_DIR).join("journal");
    if !dir.exists() {
        return Ok(0);
    }
    let mut pending = 0;
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) == Some("json")
            && !path.with_extension("ack").exists()
        {
            pending += 1;
        }
    }
    Ok(pending)
}
