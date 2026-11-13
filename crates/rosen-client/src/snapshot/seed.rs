//! Seeding a workspace's tree cache (SPEC R2: a workspace-local tree cache
//! allows the watcher to start offline).
//!
//! A large tree has thousands of subtrees, and each cached tree is written
//! durably, so seeding one at a time took tens of seconds and dominated
//! workspace creation (R3: ready in under 2 s). Seeding therefore copies one
//! depth of the tree at a time in parallel, and the warm pool seeds its ready
//! workspaces before they are taken, so a take finds the cache complete.

use super::index::empty_tree;
use super::{journal, SnapshotError};
use crate::workspace::parallel;
use rosen_core::{Object, ObjectId, ObjectKind};
use rosen_store::{ObjectStore, StoreError};
use std::path::Path;
use std::sync::Mutex;

/// Copies every tree reachable from `tree` in `store` into the tree cache of
/// the workspace at `root`, as the stored bytes. Subtrees are written before
/// `tree` itself, so a cache that holds `tree` holds all of them, and calling
/// this again returns after one lookup.
pub fn seed_tree_cache(
    store: &dyn ObjectStore,
    root: &Path,
    tree: ObjectId,
) -> Result<(), SnapshotError> {
    let cache = journal::cache(root)?;
    if tree == empty_tree() || cache.contains(&tree)? {
        return Ok(());
    }
    let mut depth = vec![tree];
    let top = Mutex::new(None);
    while !depth.is_empty() {
        let next = Mutex::new(Vec::new());
        parallel::for_each::<_, SnapshotError, _>(&depth, |id| {
            let (body, children) = read_tree(store, id)?;
            next.lock().unwrap_or_else(|p| p.into_inner()).extend(children);
            if *id == tree {
                *top.lock().unwrap_or_else(|p| p.into_inner()) = Some(body);
            } else {
                cache.put_raw(ObjectKind::Tree, &body)?;
            }
            Ok(())
        })?;
        depth = next.into_inner().unwrap_or_else(|p| p.into_inner());
        // Identical directories share one tree; copy each once.
        depth.sort_unstable();
        depth.dedup();
    }
    if let Some(body) = top.into_inner().unwrap_or_else(|p| p.into_inner()) {
        cache.put_raw(ObjectKind::Tree, &body)?;
    }
    Ok(())
}

/// A stored tree's body and the IDs of its non-empty subtrees.
fn read_tree(
    store: &dyn ObjectStore,
    id: &ObjectId,
) -> Result<(Vec<u8>, Vec<ObjectId>), SnapshotError> {
    let raw = store.get_raw(id)?.ok_or(StoreError::NotFound(*id))?;
    let Object::Tree(parsed) = raw.decode().map_err(|e| StoreError::Malformed(*id, e))? else {
        return Err(StoreError::WrongKind {
            id: *id,
            expected: ObjectKind::Tree,
            found: raw.kind,
        }
        .into());
    };
    // The empty tree is never stored, so it is never read or cached.
    let children = parsed
        .entries()
        .iter()
        .filter(|entry| entry.mode.is_tree() && entry.id != empty_tree())
        .map(|entry| entry.id)
        .collect();
    Ok((raw.data, children))
}
