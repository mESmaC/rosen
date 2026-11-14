//! Reading changed files into blobs, and sending blobs and trees to the
//! server in frames it accepts.

use super::index::{Entry, Index};
use super::scan::{disk, stat_of};
use super::SnapshotError;
use crate::connection::unexpected;
use crate::{Client, Error};
use rosen_core::{FileMode, ObjectHasher, ObjectId};
use rosen_protocol::{
    BlobChunk, ObjectUpload, Request, Response, UploadKind, UploadObject, OBJECT_CHUNK_BYTES,
};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Raw bytes per `put_objects` frame. Base64 grows them by a third, which
/// keeps a full batch well inside the protocol's 64 MiB frame limit. A blob
/// larger than this goes in slices of the same size instead.
pub(super) const BATCH_BYTES: usize = OBJECT_CHUNK_BYTES;

/// How many times a large file whose length changes while it is hashed is
/// hashed again before the snapshot gives up on it for now.
const LARGE_HASH_TRIES: usize = 3;
/// Bytes read at a time while hashing a large file.
const HASH_PIECE: usize = 1 << 20;

/// A blob to send, with the file it came from.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ChangedBlob {
    pub path: String,
    /// The bytes of a blob small enough for one frame. A larger one's stay
    /// on disk, named by `large`, so a snapshot never holds a large file
    /// whole (issue #334); this is then empty.
    #[serde(default)]
    pub data: Vec<u8>,
    #[serde(skip)]
    pub large: Option<LargeBlob>,
}

/// A blob too large for one frame, read from disk slice by slice as it is
/// sent or journaled.
#[derive(Clone)]
pub struct LargeBlob {
    pub id: ObjectId,
    pub len: u64,
    /// The file that held exactly the blob's bytes when it was hashed. A
    /// later write there fails the server's or the journal's hash check,
    /// so other bytes are never stored under `id`.
    pub source: PathBuf,
}

impl ChangedBlob {
    /// The blob's length in bytes.
    pub fn size(&self) -> u64 {
        self.large
            .as_ref()
            .map_or(self.data.len() as u64, |large| large.len)
    }
}

/// What reading the candidates found.
pub struct ReadOutcome {
    /// The blobs the server may lack.
    pub changed: Vec<ChangedBlob>,
    /// Files another process holds open, left at their indexed entry.
    pub unreadable: BTreeSet<String>,
}

/// Reads each candidate into the index. The blobs returned are those the
/// server may lack: those whose content changed, and those of `unconfirmed`
/// paths, whose last upload failed. Every file is read before the index
/// changes, so a failed read leaves the index as it was and nothing is
/// half-recorded. With `defer_unreadable`, a file another process holds open
/// is skipped instead of failing the snapshot (issue #59).
pub fn read_candidates(
    root: &Path,
    index: &mut Index,
    candidates: &BTreeSet<String>,
    unconfirmed: &BTreeSet<String>,
    defer_unreadable: bool,
) -> Result<ReadOutcome, SnapshotError> {
    let paths: BTreeSet<&String> = candidates.iter().chain(unconfirmed).collect();
    let mut read = Vec::with_capacity(paths.len());
    let mut unreadable = BTreeSet::new();
    for path in paths {
        match read_file(root, path, index.files.get(path)) {
            Ok(file) => read.push((path, file)),
            // An unconfirmed path's indexed blob may be missing on the
            // server, so the tree cannot fall back to it.
            Err(SnapshotError::Io { source, .. })
                if defer_unreadable && is_held(&source) && !unconfirmed.contains(path) =>
            {
                unreadable.insert(path.clone());
            }
            Err(err) => return Err(err),
        }
    }
    let mut changed = Vec::new();
    for (path, file) in read {
        let Some((entry, blob)) = file else {
            // Deleted since the scan saw it.
            index.files.remove(path);
            continue;
        };
        let same = index
            .files
            .get(path)
            .is_some_and(|old| old.blob == entry.blob && old.mode == entry.mode);
        if !same || unconfirmed.contains(path) {
            changed.push(blob);
        }
        index.files.insert(path.clone(), entry);
    }
    Ok(ReadOutcome {
        changed,
        unreadable,
    })
}

/// Whether a read failed because another process holds the file: a running
/// executable or an exclusively opened database refuses with a sharing or
/// lock violation, and a file being deleted refuses access. Such a file can
/// be read later, so the snapshot keeps its last stored content until then,
/// as git keeps the indexed blob of a path it cannot read.
fn is_held(err: &io::Error) -> bool {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    err.kind() == io::ErrorKind::PermissionDenied
        || (cfg!(windows)
            && matches!(
                err.raw_os_error(),
                Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
            ))
}

/// Reads one file as a blob. A symlink's blob is its target, as git stores
/// it. A file too large for one frame is only hashed, its bytes left on
/// disk. `None` when the file no longer exists.
fn read_file(
    root: &Path,
    rel: &str,
    old: Option<&Entry>,
) -> Result<Option<(Entry, ChangedBlob)>, SnapshotError> {
    let path = disk(root, rel);
    let meta = match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other.map_err(|err| SnapshotError::io(&path, err))?,
    };
    let read = if meta.file_type().is_symlink() {
        fs::read_link(&path)
            .map(|target| held(target.to_string_lossy().replace('\\', "/").into_bytes()))
    } else if meta.len() > BATCH_BYTES as u64 {
        hash_large(&path)
    } else {
        fs::read(&path).map(held)
    };
    let (blob, data, large) = match read {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other.map_err(|err| SnapshotError::io(&path, err))?,
    };
    let trusted = stat_of(&meta).filter(|stat| stat.trusted_at(SystemTime::now()));
    let entry = Entry {
        mode: mode_of(&meta, old.map(|entry| entry.mode)),
        blob,
        stat: trusted,
    };
    let path = rel.to_owned();
    Ok(Some((entry, ChangedBlob { path, data, large })))
}

type ReadBlob = (ObjectId, Vec<u8>, Option<LargeBlob>);

fn held(data: Vec<u8>) -> ReadBlob {
    (ObjectId::hash_object("blob", &data), data, None)
}

/// Hashes a file too large for one frame as it streams past. The git
/// header names the length first, so a file whose length changes while it
/// is read, as a write races the snapshot, is read again.
fn hash_large(path: &Path) -> io::Result<ReadBlob> {
    let mut piece = vec![0u8; HASH_PIECE];
    for _ in 0..LARGE_HASH_TRIES {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut hasher = ObjectHasher::new("blob", len);
        loop {
            match file.read(&mut piece) {
                Ok(0) => break,
                Ok(read) => hasher.update(&piece[..read]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        if let Some(id) = hasher.finish() {
            let source = path.to_owned();
            return Ok((id, Vec::new(), Some(LargeBlob { id, len, source })));
        }
    }
    let why = format!("{} kept changing while it was read", path.display());
    Err(io::Error::other(why))
}

/// Windows has no executable bit, and R3 writes symlinks there as plain
/// files, so a tracked file keeps the mode its tree gave it.
#[cfg(windows)]
fn mode_of(meta: &fs::Metadata, old: Option<FileMode>) -> FileMode {
    if meta.file_type().is_symlink() {
        return FileMode::Symlink;
    }
    old.unwrap_or(FileMode::Regular)
}

#[cfg(not(windows))]
fn mode_of(meta: &fs::Metadata, _old: Option<FileMode>) -> FileMode {
    use std::os::unix::fs::PermissionsExt;
    if meta.file_type().is_symlink() {
        FileMode::Symlink
    } else if meta.permissions().mode() & 0o111 != 0 {
        FileMode::Executable
    } else {
        FileMode::Regular
    }
}

/// What an upload sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sent {
    pub blobs: u64,
    pub trees: u64,
    pub bytes: u64,
}

/// Sends blobs, then trees children first, so the server never sees a tree
/// before what it names. Blobs too large for a frame go first, in slices.
pub async fn send_objects(
    client: &Client,
    blobs: &[ChangedBlob],
    trees: &[(ObjectId, Vec<u8>)],
) -> Result<Sent, Error> {
    // A record journaled before #334 can still hold a large blob's bytes.
    let (large, small): (Vec<&ChangedBlob>, Vec<&ChangedBlob>) = blobs
        .iter()
        .partition(|blob| blob.size() > BATCH_BYTES as u64);
    let mut sent = Sent::default();
    for blob in large {
        let slices = match &blob.large {
            Some(large) => Slices::OnDisk(large),
            None => Slices::Held(&blob.data),
        };
        put_large(client, slices).await?;
        sent.blobs += 1;
        sent.bytes += blob.size();
    }
    let objects = small
        .into_iter()
        .map(|blob| (UploadKind::Blob, &blob.data))
        .chain(trees.iter().map(|(_, body)| (UploadKind::Tree, body)));
    let mut batch = ObjectUpload::default();
    let mut batch_bytes = 0;
    for (kind, data) in objects {
        if batch_bytes + data.len() > BATCH_BYTES && !batch.objects.is_empty() {
            put(client, std::mem::take(&mut batch)).await?;
            batch_bytes = 0;
        }
        batch_bytes += data.len();
        sent.bytes += data.len() as u64;
        match kind {
            UploadKind::Blob => sent.blobs += 1,
            UploadKind::Tree => sent.trees += 1,
        }
        batch.objects.push(UploadObject {
            kind,
            data: data.clone(),
        });
    }
    if !batch.objects.is_empty() {
        put(client, batch).await?;
    }
    Ok(sent)
}

/// Where a large blob's slices are read from.
enum Slices<'a> {
    Held(&'a [u8]),
    OnDisk(&'a LargeBlob),
}

impl Slices<'_> {
    fn id(&self) -> ObjectId {
        match self {
            Slices::Held(data) => ObjectId::hash_object("blob", data),
            Slices::OnDisk(large) => large.id,
        }
    }

    fn len(&self) -> u64 {
        match self {
            Slices::Held(data) => data.len() as u64,
            Slices::OnDisk(large) => large.len,
        }
    }

    /// The slice at `offset`, at most one frame long. Read off the async
    /// workers. A file cut short since it was hashed gives a short slice,
    /// and the server then refuses the blob as not hashing to its ID.
    async fn slice(&self, offset: u64) -> Result<Vec<u8>, Error> {
        let end = offset.saturating_add(BATCH_BYTES as u64).min(self.len());
        let large = match self {
            Slices::Held(data) => return Ok(data[offset as usize..end as usize].to_vec()),
            Slices::OnDisk(large) => large.source.clone(),
        };
        let read = tokio::task::spawn_blocking(move || {
            let mut file = File::open(&large)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut data = Vec::with_capacity((end - offset) as usize);
            file.take(end - offset).read_to_end(&mut data)?;
            Ok::<_, io::Error>(data)
        });
        read.await.map_err(io::Error::other)?.map_err(Error::Io)
    }
}

/// Uploads one blob in frame-sized slices. A fresh upload name per call keeps
/// two uploads of the same blob from writing into each other's staging.
async fn put_large(client: &Client, slices: Slices<'_>) -> Result<(), Error> {
    let (id, total) = (slices.id(), slices.len());
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let upload = format!("{}-{:x}-{nanos:x}", &id.to_hex()[..16], std::process::id());
    let mut offset = 0;
    loop {
        let data = slices.slice(offset).await?;
        let end = offset + data.len() as u64;
        let chunk = BlobChunk {
            upload: upload.clone(),
            id,
            total,
            offset,
            data,
        };
        let stored = match client.request(Request::PutBlobChunk(chunk)).await? {
            Response::BlobChunkStored(stored) => stored,
            other => return Err(unexpected(other)),
        };
        // The server may already hold the blob, and then says so early.
        if stored.stored {
            return Ok(());
        }
        if end >= total || end == offset {
            return Err(Error::Unexpected(format!(
                "the server did not store blob {id} after its last slice"
            )));
        }
        offset = end;
    }
}

async fn put(client: &Client, batch: ObjectUpload) -> Result<(), Error> {
    match client.request(Request::PutObjects(batch)).await? {
        Response::ObjectsStored(_) => Ok(()),
        other => Err(unexpected(other)),
    }
}
