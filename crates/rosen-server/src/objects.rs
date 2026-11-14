//! Object reads on the server (SPEC D8): machines' caches fill their stores
//! from these answers.

use crate::ServerState;
use rosen_core::ObjectId;
use rosen_protocol::{
    EnsureObjects, ErrorBody, ErrorCode, ObjectBatch, ObjectChunk, ObjectChunkQuery, ObjectQuery,
    ObjectsLocal, Outcome, Response, WireObject, OBJECT_CHUNK_BYTES,
};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
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

/// Answers `Request::GetObjectChunk`. Each slice reads the whole object
/// again: large objects are rare, and keeping no per-reader state means a
/// reader that disappears leaves nothing behind.
pub async fn get_chunk(state: &Arc<ServerState>, query: ObjectChunkQuery) -> Outcome {
    let objects_dir = state.config.objects_dir();
    blocking(move || {
        let store = LooseObjectStore::open(objects_dir)?;
        Ok(match read_chunk(&store, &query)? {
            Some(chunk) => Outcome::Ok(Response::ObjectChunk(chunk)),
            None => {
                let message = format!("the server does not hold {}", query.id);
                Outcome::Error(ErrorBody::new(ErrorCode::NotFound, message))
            }
        })
    })
    .await
}

/// One slice of a stored object, at most [`OBJECT_CHUNK_BYTES`] long.
pub fn read_chunk(
    store: &dyn ObjectStore,
    query: &ObjectChunkQuery,
) -> Result<Option<ObjectChunk>, StoreError> {
    let Some(raw) = store.get_raw(&query.id)? else {
        return Ok(None);
    };
    let total = raw.data.len();
    let start = usize::try_from(query.offset)
        .unwrap_or(usize::MAX)
        .min(total);
    let len = usize::try_from(query.len)
        .unwrap_or(usize::MAX)
        .min(OBJECT_CHUNK_BYTES);
    let end = start.saturating_add(len).min(total);
    Ok(Some(ObjectChunk {
        kind: raw.kind.into(),
        total: total as u64,
        data: raw.data[start..end].to_vec(),
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
/// object too large for one frame is listed in `large` instead. Its size is
/// learnt by reading it, which costs extra only for those rare objects,
/// not a second open for every small one.
pub fn read_batch(store: &dyn ObjectStore, ids: &[ObjectId]) -> Result<ObjectBatch, StoreError> {
    let mut batch = ObjectBatch::default();
    let mut bytes = 0usize;
    for id in ids {
        match store.get_raw(id)? {
            Some(raw) if raw.data.len() > OBJECT_CHUNK_BYTES => batch.large.push(*id),
            Some(raw) => {
                if bytes > 0 && bytes + raw.data.len() > BATCH_BYTES {
                    break;
                }
                bytes += raw.data.len();
                batch.objects.push(WireObject {
                    kind: raw.kind.into(),
                    data: raw.data,
                });
            }
            None => batch.missing.push(*id),
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
