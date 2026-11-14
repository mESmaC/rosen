//! Chunked blob uploads (SPEC R2): a file too large for one `PutObjects`
//! frame arrives as `PutBlobChunk` slices.
//!
//! Each upload is staged in its own file under the data directory until its
//! last byte arrives. Only then is it hashed and stored, so the object store
//! never holds a partial blob, and a blob whose bytes do not hash to the
//! claimed ID is refused. A client that disappears mid-upload leaves its
//! staging file behind; the next upload to start sweeps out stale ones.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rosen_core::{ObjectId, ObjectKind};
use rosen_protocol::{BlobChunk, BlobChunkStored, ErrorBody, ErrorCode, Outcome, Response};
use rosen_store::{LooseObjectStore, ObjectStore};

use crate::ServerState;

/// A staging file untouched this long belongs to an abandoned upload. A
/// live upload writes a slice every few seconds at worst.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);
/// Upload names become file names, so they are kept short and plain.
const MAX_UPLOAD_NAME: usize = 64;

/// Answers `Request::PutBlobChunk` on the blocking pool: it writes and
/// hashes up to a whole large file.
pub async fn put_chunk(state: &Arc<ServerState>, chunk: BlobChunk) -> Outcome {
    let staging = state.config.uploads_dir();
    let objects = state.config.objects_dir();
    let work = move || {
        let store = LooseObjectStore::open(objects).map_err(internal)?;
        store_chunk(&staging, &store, &chunk)
    };
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(stored)) => Outcome::Ok(Response::BlobChunkStored(stored)),
        Ok(Err(body)) => Outcome::Error(body),
        Err(join) => Outcome::Error(internal(join)),
    }
}

/// Appends one slice to its upload, and stores the blob once it is whole.
pub fn store_chunk(
    staging: &Path,
    store: &dyn ObjectStore,
    chunk: &BlobChunk,
) -> Result<BlobChunkStored, ErrorBody> {
    let path = staging_path(staging, &chunk.upload)?;
    // Another client may have stored the same blob meanwhile; the rest of
    // this upload would only repeat it.
    if store.contains(&chunk.id).map_err(internal)? {
        let _ = fs::remove_file(&path);
        return Ok(BlobChunkStored {
            received: chunk.total,
            stored: true,
        });
    }
    let received = chunk.offset.saturating_add(chunk.data.len() as u64);
    if received > chunk.total {
        let why = format!(
            "chunk ends at {received}, past the blob's {} bytes",
            chunk.total
        );
        return Err(ErrorBody::new(ErrorCode::InvalidArgument, why));
    }
    append(staging, &path, chunk)?;
    if received < chunk.total {
        return Ok(BlobChunkStored {
            received,
            stored: false,
        });
    }
    let stored = finish(store, &path, chunk.id);
    let _ = fs::remove_file(&path);
    stored.map(|()| BlobChunkStored {
        received,
        stored: true,
    })
}

/// Writes the slice where the previous one ended. Offset 0 starts the
/// upload over, so a client that lost an answer can always restart.
fn append(staging: &Path, path: &Path, chunk: &BlobChunk) -> Result<(), ErrorBody> {
    let mut file = if chunk.offset == 0 {
        fs::create_dir_all(staging).map_err(internal)?;
        sweep_stale(staging);
        File::create(path).map_err(internal)?
    } else {
        let file = match OpenOptions::new().append(true).open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let why = format!("upload {} is unknown; restart it at offset 0", chunk.upload);
                return Err(ErrorBody::new(ErrorCode::Refused, why));
            }
            Err(err) => return Err(internal(err)),
        };
        let held = file.metadata().map_err(internal)?.len();
        if held != chunk.offset {
            let why = format!(
                "upload {} holds {held} bytes, not {}; resend from {held}",
                chunk.upload, chunk.offset
            );
            return Err(ErrorBody::new(ErrorCode::Refused, why));
        }
        file
    };
    file.write_all(&chunk.data).map_err(internal)
}

/// Stores the staged bytes if they are the blob the client named.
fn finish(store: &dyn ObjectStore, path: &Path, id: ObjectId) -> Result<(), ErrorBody> {
    let data = fs::read(path).map_err(internal)?;
    if ObjectId::hash_object("blob", &data) != id {
        let why = format!("the uploaded bytes do not hash to blob {id}; upload it again");
        return Err(ErrorBody::new(ErrorCode::InvalidArgument, why));
    }
    store.put_raw(ObjectKind::Blob, &data).map_err(internal)?;
    Ok(())
}

fn staging_path(staging: &Path, upload: &str) -> Result<PathBuf, ErrorBody> {
    let plain = upload
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if upload.is_empty() || upload.len() > MAX_UPLOAD_NAME || !plain {
        let why = "an upload name is 1 to 64 letters, digits or '-'";
        return Err(ErrorBody::new(ErrorCode::InvalidArgument, why));
    }
    Ok(staging.join(format!("{upload}.part")))
}

/// Best effort: a staging file that cannot be removed now is tried again
/// when the next upload starts.
fn sweep_stale(staging: &Path) {
    let Ok(entries) = fs::read_dir(staging) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() > STALE_AFTER);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn internal(err: impl ToString) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, err.to_string())
}
