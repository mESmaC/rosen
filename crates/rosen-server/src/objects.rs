//! Object reads on the server (SPEC D8): machines' caches fill their stores
//! from these answers.

use crate::ServerState;
use rosen_core::ObjectId;
use rosen_protocol::{
    EnsureObjects, ErrorBody, ErrorCode, ObjectBatch, ObjectQuery, ObjectsLocal, Outcome, Response,
    WireObject,
};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use std::sync::Arc;

/// Object bytes per answer. Base64 adds a third, so a full batch stays well
/// under the 64 MiB frame limit. One object is always sent even when it
/// alone is larger, so a big blob is never deferred forever.
pub const BATCH_BYTES: usize = 16 * 1024 * 1024;

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
/// in neither list of the answer, which tells the reader to ask again.
pub fn read_batch(store: &dyn ObjectStore, ids: &[ObjectId]) -> Result<ObjectBatch, StoreError> {
    let mut batch = ObjectBatch::default();
    let mut bytes = 0usize;
    for id in ids {
        match store.get_raw(id)? {
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
