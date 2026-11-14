//! Reading changed files into blobs, and sending blobs and trees to the
//! server in frames it accepts.

use super::index::{Entry, Index};
use super::scan::{disk, stat_of};
use super::SnapshotError;
use crate::connection::unexpected;
use crate::{Client, Error};
use rosen_core::{FileMode, ObjectId};
use rosen_protocol::{
    BlobChunk, ObjectUpload, Request, Response, UploadKind, UploadObject, OBJECT_CHUNK_BYTES,
};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Raw bytes per `put_objects` frame. Base64 grows them by a third, which
/// keeps a full batch well inside the protocol's 64 MiB frame limit. A blob
/// larger than this goes in slices of the same size instead.
const BATCH_BYTES: usize = OBJECT_CHUNK_BYTES;
/// A timestamp this close to the read is not trusted to detect later writes:
/// a second write in the same clock tick would leave it unchanged.
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// A blob to send, with the file it came from.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ChangedBlob {
    pub path: String,
    pub data: Vec<u8>,
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
        let Some((entry, data)) = file else {
            // Deleted since the scan saw it.
            index.files.remove(path);
            continue;
        };
        let same = index
            .files
            .get(path)
            .is_some_and(|old| old.blob == entry.blob && old.mode == entry.mode);
        if !same || unconfirmed.contains(path) {
            changed.push(ChangedBlob {
                path: path.clone(),
                data,
            });
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
/// it. `None` when the file no longer exists.
fn read_file(
    root: &Path,
    rel: &str,
    old: Option<&Entry>,
) -> Result<Option<(Entry, Vec<u8>)>, SnapshotError> {
    let path = disk(root, rel);
    let meta = match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other.map_err(|err| SnapshotError::io(&path, err))?,
    };
    let read = if meta.file_type().is_symlink() {
        fs::read_link(&path).map(|target| target.to_string_lossy().replace('\\', "/").into_bytes())
    } else {
        fs::read(&path)
    };
    let data = match read {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other.map_err(|err| SnapshotError::io(&path, err))?,
    };
    let trusted = stat_of(&meta).filter(|stat| {
        SystemTime::now()
            .duration_since(stat.modified)
            .is_ok_and(|age| age > RACY_WINDOW)
    });
    let entry = Entry {
        mode: mode_of(&meta, old.map(|entry| entry.mode)),
        blob: ObjectId::hash_object("blob", &data),
        stat: trusted,
    };
    Ok(Some((entry, data)))
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
    let (large, small): (Vec<&ChangedBlob>, Vec<&ChangedBlob>) = blobs
        .iter()
        .partition(|blob| blob.data.len() > BATCH_BYTES);
    let mut sent = Sent::default();
    for blob in large {
        put_large(client, &blob.data).await?;
        sent.blobs += 1;
        sent.bytes += blob.data.len() as u64;
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

/// Uploads one blob in frame-sized slices. A fresh upload name per call keeps
/// two uploads of the same blob from writing into each other's staging.
async fn put_large(client: &Client, data: &[u8]) -> Result<(), Error> {
    let id = ObjectId::hash_object("blob", data);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let upload = format!("{}-{:x}-{nanos:x}", &id.to_hex()[..16], std::process::id());
    let mut offset = 0;
    loop {
        let end = (offset + BATCH_BYTES).min(data.len());
        let chunk = BlobChunk {
            upload: upload.clone(),
            id,
            total: data.len() as u64,
            offset: offset as u64,
            data: data[offset..end].to_vec(),
        };
        let stored = match client.request(Request::PutBlobChunk(chunk)).await? {
            Response::BlobChunkStored(stored) => stored,
            other => return Err(unexpected(other)),
        };
        // The server may already hold the blob, and then says so early.
        if stored.stored {
            return Ok(());
        }
        if end == data.len() {
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
