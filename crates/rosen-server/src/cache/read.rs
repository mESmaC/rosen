//! Chunk reads share the cache's repair path instead of bypassing its store.

use super::{fill, session::LinkSlot, CacheState};
use rosen_protocol::{
    ErrorBody, ErrorCode, ObjectChunk, ObjectChunkQuery, Outcome, Request, Response,
};
use rosen_store::StoreError;

pub async fn get_chunk(
    state: &CacheState,
    links: &LinkSlot,
    query: ObjectChunkQuery,
) -> Result<ObjectChunk, ErrorBody> {
    match read_local(state, query.clone()).await {
        Ok(Some(chunk)) => Ok(chunk),
        Ok(None) => {
            // One slice cannot establish an object's hash, so an absent
            // object is still forwarded without persisting partial bytes.
            let link = links.get().await?;
            match link.request(Request::GetObjectChunk(query)).await {
                Outcome::Ok(Response::ObjectChunk(chunk)) => Ok(chunk),
                Outcome::Ok(_) => Err(ErrorBody::new(
                    ErrorCode::Internal,
                    "the server answered a chunk read with something else",
                )),
                Outcome::Error(error) => Err(error),
            }
        }
        Err(StoreError::Corrupt { .. }) => {
            // Repair the loose object too: workspaces read that copy directly.
            fill::ensure(&state.store, &state.partial, links, query.id, &[]).await?;
            read_local(state, query.clone())
                .await
                .map_err(store_error)?
                .ok_or_else(|| {
                    ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("the cache no longer holds {}", query.id),
                    )
                })
        }
        Err(error) => Err(store_error(error)),
    }
}

/// Large objects use the same checked disk copy as authoritative reads,
/// avoiding another full inflate and hash for each requested slice.
async fn read_local(
    state: &CacheState,
    query: ObjectChunkQuery,
) -> Result<Option<ObjectChunk>, StoreError> {
    let store = state.store.clone();
    let slices = state.config.slices_dir();
    tokio::task::spawn_blocking(move || crate::objects::read_chunk(&store, &slices, &query))
        .await
        .map_err(std::io::Error::other)?
}

fn store_error(error: StoreError) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, error.to_string())
}
