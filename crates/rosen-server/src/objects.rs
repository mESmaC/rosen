//! Object reads on the server (SPEC D8): machines' caches fill their stores
//! from these answers.

use crate::ServerState;
use rosen_core::ObjectId;
use rosen_protocol::{
    EnsureObjects, ErrorBody, ErrorCode, ObjectBatch, ObjectChunk, ObjectChunkQuery, ObjectQuery,
    ObjectsLocal, Outcome, Response, WireObject, OBJECT_CHUNK_BYTES,
};
use rosen_store::{LooseObjectStore, ObjectStore, ReadWithin, StoreError};
use std::path::Path;
use std::sync::Arc;

/// Object bytes per answer. Base64 adds a third, so a full batch stays well
/// under the 64 MiB frame limit. One object up to this size is always sent,
/// so it is never deferred forever; a larger one is listed as `large` and
/// read in slices.
pub const BATCH_BYTES: usize = OBJECT_CHUNK_BYTES;

/// Answers `Request::GetObjects`.
pub async fn get(state: &Arc<ServerState>, query: ObjectQuery) -> Outcome {
    let objects_dir = state.config.objects_dir();
    blocking(move || {
        let store = LooseObjectStore::open(objects_dir)?;
        let batch = read_batch(&store, &query.ids)?;
        Ok(Outcome::Ok(Response::Objects(batch)))
    })
    .await
}

/// Answers `Request::GetObjectChunk`.
pub async fn get_chunk(state: &Arc<ServerState>, query: ObjectChunkQuery) -> Outcome {
    let objects_dir = state.config.objects_dir();
    let slices_dir = state.config.slices_dir();
    blocking(move || {
        let store = LooseObjectStore::open(objects_dir)?;
        Ok(match read_chunk(&store, &slices_dir, &query)? {
            Some(chunk) => Outcome::Ok(Response::ObjectChunk(chunk)),
            None => {
                let message = format!("the server does not hold {}", query.id);
                Outcome::Error(ErrorBody::new(ErrorCode::NotFound, message))
            }
        })
    })
    .await
}

/// One slice of a stored object, at most [`OBJECT_CHUNK_BYTES`] long. A
/// small object is read whole; a large one from its checked copy in
/// `slices_dir`, so no slice holds or inflates the whole object.
pub fn read_chunk(
    store: &LooseObjectStore,
    slices_dir: &Path,
    query: &ObjectChunkQuery,
) -> Result<Option<ObjectChunk>, StoreError> {
    let len = query.len.min(OBJECT_CHUNK_BYTES as u64);
    let (kind, total, data) = match store.get_raw_within(&query.id, OBJECT_CHUNK_BYTES as u64)? {
        ReadWithin::Missing => return Ok(None),
        ReadWithin::Read(raw) => {
            let total = raw.data.len();
            let start = usize::try_from(query.offset)
                .unwrap_or(usize::MAX)
                .min(total);
            let end = start.saturating_add(len as usize).min(total);
            (raw.kind, total as u64, raw.data[start..end].to_vec())
        }
        ReadWithin::TooLarge(total) => {
            let slice =
                crate::object_slices::read(store, slices_dir, &query.id, total, query.offset, len)?;
            let Some((kind, data)) = slice else {
                return Ok(None);
            };
            (kind, total, data)
        }
    };
    Ok(Some(ObjectChunk {
        kind: kind.into(),
        total,
        data,
    }))
}

/// Answers `Request::EnsureObjects`. The server's store is authoritative
/// and a stored tree is always complete, so the root's presence is enough.
pub async fn ensure(state: &Arc<ServerState>, ensure: EnsureObjects) -> Outcome {
    let objects_dir = state.config.objects_dir();
    blocking(move || {
        let store = LooseObjectStore::open(objects_dir)?;
        if store.contains(&ensure.root)? {
            return Ok(Outcome::Ok(Response::ObjectsLocal(ObjectsLocal {
                fetched: 0,
            })));
        }
        let message = format!("the server does not hold {}", ensure.root);
        Ok(Outcome::Error(ErrorBody::new(ErrorCode::NotFound, message)))
    })
    .await
}

/// Reads `ids` in order until the byte budget is spent. IDs left over are
/// in none of the answer's lists, which tells the reader to ask again. An
/// object too large for one frame is listed in `large` instead, known from
/// its header alone, so it is never read here.
pub fn read_batch(store: &LooseObjectStore, ids: &[ObjectId]) -> Result<ObjectBatch, StoreError> {
    let mut batch = ObjectBatch::default();
    let mut bytes = 0usize;
    for id in ids {
        match store.get_raw_within(id, OBJECT_CHUNK_BYTES as u64)? {
            ReadWithin::TooLarge(_) => batch.large.push(*id),
            ReadWithin::Read(raw) => {
                if bytes > 0 && bytes + raw.data.len() > BATCH_BYTES {
                    break;
                }
                bytes += raw.data.len();
                batch.objects.push(WireObject {
                    kind: raw.kind.into(),
                    data: raw.data,
                });
            }
            ReadWithin::Missing => batch.missing.push(*id),
        }
    }
    Ok(batch)
}

/// Store I/O runs on the blocking pool, so it never stalls the async workers
/// that carry every other connection.
async fn blocking(work: impl FnOnce() -> Result<Outcome, StoreError> + Send + 'static) -> Outcome {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, err.to_string())),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}
