//! Resend only requested objects, including unchanged workspace files (#363).

use super::{index::Index, upload, Prepared, Sent, Snapshotter};
use crate::{Client, Error};
use rosen_core::{ObjectId, ObjectKind};
use rosen_store::{MemoryObjectStore, ObjectStore};
use std::collections::HashSet;

pub(super) async fn resend(
    client: &Client,
    state: &mut Snapshotter,
    prepared: &mut Prepared,
    needed: &HashSet<ObjectId>,
) -> Result<Sent, Error> {
    let trees: Vec<_> = prepared
        .trees
        .iter()
        .filter(|(id, _)| needed.contains(id))
        .cloned()
        .collect();
    let store = MemoryObjectStore::new();
    for (_, body) in &prepared.trees {
        store
            .put_raw(ObjectKind::Tree, body)
            .map_err(super::SnapshotError::from)?;
    }
    let index = Index::from_tree(&store, prepared.root)?;
    let mut blobs = Vec::new();
    for id in needed
        .iter()
        .filter(|id| !trees.iter().any(|(tree, _)| tree == *id))
    {
        if let Some(blob) = prepared.blobs.iter().find(|blob| blob_id(blob) == *id) {
            blobs.push(blob.clone());
            continue;
        }
        let paths: Vec<_> = index
            .files
            .iter()
            .filter(|(_, entry)| entry.blob == *id)
            .filter(|(path, _)| state.sparse.includes(path.as_bytes()))
            .map(|(path, _)| path.clone())
            .collect();
        // A write racing this repair must be read by the next snapshot too.
        state.unconfirmed.extend(paths.iter().cloned());
        let root = state.root.clone();
        let id = *id;
        let blob = tokio::task::spawn_blocking(move || read_matching(&root, &paths, id))
            .await
            .map_err(std::io::Error::other)??;
        // A disconnect journals the prepared snapshot. Keep repair bytes
        // there too, before a later workspace edit can remove their source.
        prepared.blobs.push(blob.clone());
        blobs.push(blob);
    }
    upload::send_objects(client, &blobs, &trees).await
}

fn blob_id(blob: &upload::ChangedBlob) -> ObjectId {
    blob.large.as_ref().map_or_else(
        || ObjectId::hash_object("blob", &blob.data),
        |large| large.id,
    )
}

fn read_matching(
    root: &std::path::Path,
    paths: &[String],
    id: ObjectId,
) -> Result<upload::ChangedBlob, Error> {
    for path in paths {
        if let Some((entry, blob)) = upload::read_file(root, path, None)? {
            // Never relabel newer workspace bytes as an older missing object.
            if entry.blob == id {
                return Ok(blob);
            }
        }
    }
    let mut body = rosen_protocol::ErrorBody::new(
        rosen_protocol::ErrorCode::Internal,
        format!("cannot repair {id}: no matching workspace bytes are available"),
    );
    body.objects_needed.push(id);
    Err(Error::Server(body))
}
