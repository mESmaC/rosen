//! The offline journal (SPEC R2): snapshots captured while the server is
//! unreachable, kept until the server acknowledges them, then removed.
//!
//! A record holds only ids: its blobs go to a workspace-local object store
//! and its trees to the tree cache, so a record stays small however large
//! the files are, and reading the journal never parses file contents.
use super::upload::ChangedBlob;
use super::{Prepared, SnapshotError, STATE_DIR};
use rosen_core::{ObjectId, ObjectKind};
use rosen_store::{LooseObjectStore, ObjectStore};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

/// A captured snapshot waiting for the server.
pub(super) struct Record {
    pub prepared: Prepared,
    pub message: Option<String>,
}

/// A record as stored on disk: object ids only.
#[derive(Serialize, Deserialize)]
struct StoredRecord {
    blobs: Vec<(String, ObjectId)>,
    root: ObjectId,
    all_trees: Vec<ObjectId>,
    removed: usize,
    message: Option<String>,
}

/// Records written before blobs moved out of the record carried the whole
/// `Prepared`, file bytes included. They are still read, so nothing captured
/// by an earlier build is lost.
#[derive(Deserialize)]
struct InlineRecord {
    prepared: Prepared,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OnDisk {
    Stored(StoredRecord),
    Inline(InlineRecord),
}

pub(super) struct Journal {
    dir: PathBuf,
    /// Held shared for the snapshotter's life, so a deletion (which needs it
    /// exclusive) cannot start while any snapshotter has the workspace open.
    presence: File,
    /// Taken exclusive around each capture and replay. Writers queue on it
    /// instead of failing, so `rosen commit` runs beside a watcher.
    writer: PathBuf,
}

/// Holds the journal's writer lock until dropped.
pub(super) struct WriterLease {
    _file: File,
}

impl Journal {
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join(STATE_DIR).join("journal");
        fs::create_dir_all(&dir)?;
        // Windows refuses removing a byte-range-locked file, so the leases
        // must live outside the workspace that R4 deletes while holding them.
        let canonical = fs::canonicalize(root)?;
        let locks = canonical
            .parent()
            .ok_or_else(|| io::Error::other("workspace has no parent"))?
            .join(".rosen-locks");
        fs::create_dir_all(&locks)?;
        let key = ObjectId::hash_object("blob", canonical.to_string_lossy().as_bytes());
        let presence = lock_file(&locks.join(format!("{key}.lock")))?;
        presence.try_lock_shared().map_err(|_| {
            io::Error::new(io::ErrorKind::WouldBlock, "the workspace is being deleted")
        })?;
        Ok(Self {
            dir,
            presence,
            writer: locks.join(format!("{key}.writer.lock")),
        })
    }

    /// Waits for the writer lock. Blocking.
    pub fn writer(&self) -> io::Result<WriterLease> {
        let file = lock_file(&self.writer)?;
        file.lock()?;
        Ok(WriterLease { _file: file })
    }

    /// Takes the workspace for deletion: fails while any other snapshotter,
    /// such as a running watcher, has it open.
    pub fn exclusive(&self) -> io::Result<()> {
        self.presence.unlock()?;
        if self.presence.try_lock().is_ok() {
            return Ok(());
        }
        self.presence.try_lock_shared().map_err(io::Error::other)?;
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "another snapshot writer, such as a watcher, has the workspace open",
        ))
    }

    /// Sequence numbers of unacknowledged records, oldest first. Reads only
    /// file names. Call while holding the writer lock.
    pub fn pending(&self) -> io::Result<Vec<u64>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let Some(id) = sequence(&path) else { continue };
            match path.extension().and_then(|x| x.to_str()) {
                // An acknowledgement file comes from an earlier build, which
                // kept acknowledged records; finish removing them now.
                Some("ack") if !path.with_extension("json").exists() => fs::remove_file(&path)?,
                Some("json") if path.with_extension("ack").exists() => self.remove(id)?,
                Some("json") => ids.push(id),
                _ => {}
            }
        }
        ids.sort_unstable();
        if ids.is_empty() {
            self.clear_blobs()?;
        }
        Ok(ids)
    }

    /// The root tree a record captured, without loading its blobs.
    pub fn root_of(&self, id: u64) -> io::Result<ObjectId> {
        Ok(match self.read(id)? {
            OnDisk::Stored(record) => record.root,
            OnDisk::Inline(record) => record.prepared.root,
        })
    }

    /// Loads a record with its blob bytes and tree bodies, to send it.
    pub fn load(&self, id: u64, trees: &dyn ObjectStore) -> Result<Record, SnapshotError> {
        let stored = match self
            .read(id)
            .map_err(|err| SnapshotError::io(&self.dir, err))?
        {
            OnDisk::Inline(record) => {
                return Ok(Record {
                    prepared: record.prepared,
                    message: record.message,
                })
            }
            OnDisk::Stored(stored) => stored,
        };
        let blobs_store = self.blobs()?;
        let blobs = stored
            .blobs
            .into_iter()
            .map(|(path, id)| {
                Ok(ChangedBlob {
                    path,
                    data: raw(&blobs_store, &id)?,
                })
            })
            .collect::<Result<Vec<_>, SnapshotError>>()?;
        let tree_bodies = stored
            .all_trees
            .iter()
            .map(|id| Ok((*id, raw(trees, id)?)))
            .collect::<Result<Vec<_>, SnapshotError>>()?;
        Ok(Record {
            prepared: Prepared {
                blobs,
                trees: tree_bodies,
                root: stored.root,
                all_trees: stored.all_trees,
                removed: stored.removed,
            },
            message: stored.message,
        })
    }

    /// Stores a record after `last`. Its trees must already be in the tree
    /// cache ([`save_trees`]). Call while holding the writer lock.
    pub fn append(&self, last: Option<u64>, record: &Record) -> Result<(), SnapshotError> {
        let blobs_store = self.blobs()?;
        let mut blobs = Vec::with_capacity(record.prepared.blobs.len());
        for blob in &record.prepared.blobs {
            let id = blobs_store.put_raw(ObjectKind::Blob, &blob.data)?;
            blobs.push((blob.path.clone(), id));
        }
        let stored = StoredRecord {
            blobs,
            root: record.prepared.root,
            all_trees: record.prepared.all_trees.clone(),
            removed: record.prepared.removed,
            message: record.message.clone(),
        };
        let bytes = serde_json::to_vec(&stored).map_err(io::Error::other);
        let next = last.map_or(1, |id| id + 1);
        bytes
            .and_then(|bytes| self.publish(next, &bytes))
            .map_err(|err| SnapshotError::io(&self.dir, err))
    }

    /// Removes a record the server has stored.
    pub fn remove(&self, id: u64) -> io::Result<()> {
        fs::remove_file(self.dir.join(format!("{id}.json")))?;
        match fs::remove_file(self.dir.join(format!("{id}.ack"))) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    fn read(&self, id: u64) -> io::Result<OnDisk> {
        let bytes = fs::read(self.dir.join(format!("{id}.json")))?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    fn publish(&self, id: u64, bytes: &[u8]) -> io::Result<()> {
        // A stale acknowledgement under a reused number would hide the record.
        match fs::remove_file(self.dir.join(format!("{id}.ack"))) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
        let temp = self.dir.join("pending.tmp");
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(temp, self.dir.join(format!("{id}.json")))
    }

    fn blobs(&self) -> Result<LooseObjectStore, SnapshotError> {
        Ok(LooseObjectStore::open(self.dir.join("objects"))?)
    }

    /// Blob bytes are needed only while a record waits, so they go with the
    /// last record. Content addressing means records cannot share them safely
    /// any other way without reference counts.
    fn clear_blobs(&self) -> io::Result<()> {
        match fs::remove_dir_all(self.dir.join("objects")) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }
}

fn lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn sequence(path: &Path) -> Option<u64> {
    path.file_stem()?.to_str()?.parse().ok()
}

fn raw(store: &dyn ObjectStore, id: &ObjectId) -> Result<Vec<u8>, SnapshotError> {
    match store.get_raw(id)? {
        Some(object) => Ok(object.data),
        None => Err(SnapshotError::io(
            Path::new(".rosen/journal"),
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("journal object {id} is missing"),
            ),
        )),
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
