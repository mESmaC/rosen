//! The offline journal (SPEC R2): snapshots captured while the server is
//! unreachable, kept until the server acknowledges them, then removed.
//!
//! A record holds only ids: its blobs go to a workspace-local object store
//! and its trees to the tree cache, so a record stays small however large
//! the files are, and reading the journal never parses file contents.
use super::upload::{ChangedBlob, LargeBlob, BATCH_BYTES};
use super::{Prepared, SnapshotError, STATE_DIR};
use rosen_core::{ChangeState, ObjectId, ObjectKind};
use rosen_protocol::ErrorBody;
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// A captured snapshot waiting for the server.
pub(super) struct Record {
    pub prepared: Prepared,
    pub message: Option<String>,
    /// A request for this record may have reached the server without its
    /// answer arriving, so the server may hold it already.
    pub unanswered: bool,
}

/// A record as stored on disk: object ids only.
#[derive(Serialize, Deserialize)]
struct StoredRecord {
    blobs: Vec<(String, ObjectId)>,
    root: ObjectId,
    all_trees: Vec<ObjectId>,
    removed: usize,
    message: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unanswered: bool,
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

/// Large journaled blobs inflated for sending, which reads them in slices
/// from a file. Kept with the blobs, and removed with them.
const INFLATED: &str = "inflated";

/// Why replay stopped: the server refused a record for good (issue #91).
/// Its name has no number and no `.json`, so it never reads as a record.
const HALT: &str = "halt";

#[derive(Serialize, Deserialize)]
pub(super) struct Halt {
    pub record: u64,
    pub refusal: ErrorBody,
    /// The change as the server showed it just after the refusal. Replay
    /// resumes only once the change differs (#273). Absent when it could
    /// not be read, and from halts written before it was recorded: those
    /// resume whenever the change is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<ChangeSeen>,
}

/// What a halted journal waits to see change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ChangeSeen {
    pub state: ChangeState,
    pub head: ObjectId,
}

pub(super) struct Journal {
    dir: PathBuf,
    /// Held shared for the snapshotter's life, so a deletion (which needs it
    /// exclusive) cannot start while any snapshotter has the workspace open.
    presence: File,
    presence_path: PathBuf,
    /// Set once [`Journal::exclusive`] holds the presence lease exclusive.
    held_exclusive: AtomicBool,
    /// Taken exclusive around each capture and replay. Writers queue on it
    /// instead of failing, so `rosen commit` runs beside a watcher.
    writer: PathBuf,
}

/// How long to retry a presence lease whose file a deletion removed while
/// it was being taken, and the pause between tries. Without POSIX delete
/// semantics, Windows keeps a removed file's name until every handle on it
/// closes, including a virus scanner's or the search indexer's, which can
/// outlast the deletion by far more than a few tries (issue #262).
const PRESENCE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const PRESENCE_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);

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
        let presence_path = locks.join(format!("{key}.lock"));
        Ok(Self {
            dir,
            presence: lock_presence(&presence_path)?,
            presence_path,
            held_exclusive: AtomicBool::new(false),
            writer: locks.join(format!("{key}.writer.lock")),
        })
    }

    /// Removes the lease files of a deleted workspace, so deletions leave
    /// nothing behind in the shared parent. Does nothing unless
    /// [`Journal::exclusive`] succeeded: no other journal holds the files
    /// then, and an opener that reached one before its removal retries
    /// ([`lock_presence`]). A leftover empty file is harmless, so this
    /// reports nothing.
    pub fn remove_leases(self) {
        if !self.held_exclusive.load(Ordering::Acquire) {
            return;
        }
        let _ = fs::remove_file(&self.writer);
        let _ = fs::remove_file(&self.presence_path);
        // The lock goes only now, after both names are gone.
        drop(self.presence);
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
            self.held_exclusive.store(true, Ordering::Release);
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
            // A halt only guards records; with none left it means nothing.
            self.resume()?;
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
                    unanswered: false,
                })
            }
            OnDisk::Stored(stored) => stored,
        };
        let blobs_store = self.blobs()?;
        let blobs = stored
            .blobs
            .into_iter()
            .map(|(path, id)| self.load_blob(&blobs_store, path, id))
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
            unanswered: stored.unanswered,
        })
    }

    /// Stores a record after `last`. Its trees must already be in the tree
    /// cache ([`save_trees`]). Call while holding the writer lock.
    pub fn append(&self, last: Option<u64>, record: &Record) -> Result<(), SnapshotError> {
        let blobs_store = self.blobs()?;
        let mut blobs = Vec::with_capacity(record.prepared.blobs.len());
        for blob in &record.prepared.blobs {
            let id = match &blob.large {
                Some(large) => journal_large(&blobs_store, large)?,
                None => blobs_store.put_raw(ObjectKind::Blob, &blob.data)?,
            };
            blobs.push((blob.path.clone(), id));
        }
        let stored = StoredRecord {
            blobs,
            root: record.prepared.root,
            all_trees: record.prepared.all_trees.clone(),
            removed: record.prepared.removed,
            message: record.message.clone(),
            unanswered: record.unanswered,
        };
        let bytes = serde_json::to_vec(&stored).map_err(io::Error::other);
        let next = last.map_or(1, |id| id + 1);
        bytes
            .and_then(|bytes| self.publish(next, &bytes))
            .map_err(|err| SnapshotError::io(&self.dir, err))
    }

    /// Stops replay after the server refused record `id` for good. The
    /// record stays, so the edit it holds survives for R4 to keep.
    pub fn halt(&self, id: u64, refusal: &ErrorBody, seen: Option<ChangeSeen>) -> io::Result<()> {
        let halt = Halt {
            record: id,
            refusal: refusal.clone(),
            seen,
        };
        let bytes = serde_json::to_vec(&halt).map_err(io::Error::other)?;
        let temp = self.dir.join("halt.tmp");
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temp, self.dir.join(HALT))
    }

    /// The refusal that stopped replay, if one did.
    pub fn halted(&self) -> io::Result<Option<Halt>> {
        match fs::read(self.dir.join(HALT)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(io::Error::other),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Lets replay run again.
    pub fn resume(&self) -> io::Result<()> {
        match fs::remove_file(self.dir.join(HALT)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    /// Notes that record `id` is about to be sent, so a replay after a lost
    /// answer or a crash asks the server before sending it again. Records
    /// from an earlier build, which carry their bytes inline, are left as is.
    pub fn mark_unanswered(&self, id: u64) -> io::Result<()> {
        let OnDisk::Stored(mut record) = self.read(id)? else {
            return Ok(());
        };
        record.unanswered = true;
        let bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
        self.publish(id, &bytes)
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

    /// A journaled blob, ready to send. A large one is inflated into a file
    /// once, checked against its ID on the way, and sent from there in
    /// slices, so replay never holds it whole (issue #334).
    fn load_blob(
        &self,
        store: &LooseObjectStore,
        path: String,
        id: ObjectId,
    ) -> Result<ChangedBlob, SnapshotError> {
        let len = match store.object_size(&id) {
            Err(StoreError::NotFound(_)) => return Err(missing(&id)),
            other => other?,
        };
        if len <= BATCH_BYTES as u64 {
            let data = raw(store, &id)?;
            return Ok(ChangedBlob {
                path,
                data,
                large: None,
            });
        }
        let dir = self.dir.join(INFLATED);
        let source = dir.join(id.to_hex());
        let inflated = fs::metadata(&source).is_ok_and(|meta| meta.len() == len);
        if !inflated {
            inflate(store, &dir, &source, &id).map_err(|err| SnapshotError::io(&dir, err))?;
        }
        let large = Some(LargeBlob { id, len, source });
        Ok(ChangedBlob {
            path,
            data: Vec::new(),
            large,
        })
    }

    /// Blob bytes are needed only while a record waits, so they go with the
    /// last record. Content addressing means records cannot share them safely
    /// any other way without reference counts.
    fn clear_blobs(&self) -> io::Result<()> {
        for dir in ["objects", INFLATED] {
            match fs::remove_dir_all(self.dir.join(dir)) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
                _ => {}
            }
        }
        Ok(())
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

/// Takes the presence lease shared. A deletion removes the lease file while
/// holding it exclusive, so an opener that reached the file first can lock
/// it once the deletion ends. That lock would guard a name no one else
/// sees, letting a later deletion run beside this writer; so the lock
/// counts only while the path still names the locked file.
fn lock_presence(path: &Path) -> io::Result<File> {
    let deadline = std::time::Instant::now() + PRESENCE_WAIT;
    loop {
        let retry = std::time::Instant::now() < deadline;
        match try_presence(path) {
            Ok(Some(file)) => return Ok(file),
            Ok(None) if retry => {}
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the workspace lease file kept being removed",
                ))
            }
            // Windows refuses opening a removed file until its last handle
            // closes. A denial that outlasts the wait has another cause,
            // such as the lock directory's access list, so it is returned
            // as itself.
            Err(err) if retry && err.kind() == io::ErrorKind::PermissionDenied => {}
            Err(err) => return Err(err),
        }
        std::thread::sleep(PRESENCE_PAUSE);
    }
}

/// One try: the shared lock on the file `path` names, or `None` when a
/// deletion removed or replaced the file meanwhile.
fn try_presence(path: &Path) -> io::Result<Option<File>> {
    let file = lock_file(path)?;
    file.try_lock_shared()
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "the workspace is being deleted"))?;
    let locked = same_file::Handle::from_file(file.try_clone()?)?;
    match same_file::Handle::from_path(path) {
        Ok(named) if named == locked => Ok(Some(file)),
        Ok(_) => Ok(None),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn sequence(path: &Path) -> Option<u64> {
    path.file_stem()?.to_str()?.parse().ok()
}

/// Copies a large blob into the journal's store from its file, checked
/// against the ID it was hashed to. A file written again since then no
/// longer matches and the record is not written: the snapshot fails, and
/// the next one reads the file at its new content.
fn journal_large(store: &LooseObjectStore, large: &LargeBlob) -> Result<ObjectId, SnapshotError> {
    let source = &large.source;
    let mut file = File::open(source).map_err(|err| SnapshotError::io(source, err))?;
    if store.put_raw_from(ObjectKind::Blob, large.id, large.len, &mut file)? {
        return Ok(large.id);
    }
    let why = "the file changed while it was being journaled";
    Err(SnapshotError::io(source, io::Error::other(why)))
}

/// Inflates a journaled blob into `source`, by way of a temporary file so
/// a cut-short copy is never taken for a whole one.
fn inflate(store: &LooseObjectStore, dir: &Path, source: &Path, id: &ObjectId) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!("{}.tmp", id.to_hex()));
    let copied = (|| {
        let mut out = io::BufWriter::new(File::create(&temp)?);
        let copied = store.copy_raw_to(id, &mut out).map_err(io::Error::other)?;
        out.into_inner().map_err(|err| err.into_error())?;
        Ok::<_, io::Error>(copied)
    })();
    match copied {
        Ok(Some(_)) => fs::rename(&temp, source),
        Ok(None) => {
            let _ = fs::remove_file(&temp);
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("journal object {id} is missing"),
            ))
        }
        Err(err) => {
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

fn missing(id: &ObjectId) -> SnapshotError {
    SnapshotError::io(
        Path::new(".rosen/journal"),
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("journal object {id} is missing"),
        ),
    )
}

fn raw(store: &dyn ObjectStore, id: &ObjectId) -> Result<Vec<u8>, SnapshotError> {
    match store.get_raw(id)? {
        Some(object) => Ok(object.data),
        None => Err(missing(id)),
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
