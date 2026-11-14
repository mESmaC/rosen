//! Seeding a workspace's tree cache (SPEC R2: a workspace-local tree cache
//! allows the watcher to start offline).
//!
//! A large tree has thousands of subtrees, and each cached tree is written
//! durably, so seeding one at a time took tens of seconds and dominated
//! workspace creation (R3: ready in under 2 s). Seeding therefore reads and
//! writes one depth of the tree at a time in parallel, and the warm pool seeds
//! its ready workspaces before they are taken, so a take finds the cache
//! complete.
//!
//! After a land only a few directories change, so seeding the new root skips
//! every subtree the cache already holds (R3: reading only the directories
//! that changed). That is sound because every tree is written after all of
//! its subtrees: a cached tree implies its whole subtree is cached.

use super::index::empty_tree;
use super::{journal, SnapshotError};
use crate::workspace::parallel;
use rosen_core::{Object, ObjectId, ObjectKind};
use rosen_store::{LooseObjectStore, ObjectStore, StoreError};
use std::path::Path;
use std::sync::Mutex;

/// Copies every tree reachable from `tree` in `store` into the tree cache of
/// the workspace at `root`, as the stored bytes, reading only trees the cache
/// lacks. Subtrees are written before the trees that hold them, so a cache
/// that holds `tree` holds all of them, and calling this again returns after
/// one lookup.
pub fn seed_tree_cache(
    store: &dyn ObjectStore,
    root: &Path,
    tree: ObjectId,
) -> Result<(), SnapshotError> {
    let cache = journal::cache(root)?;
    if tree == empty_tree() || cache.contains(&tree)? {
        return Ok(());
    }
    let levels = read_uncached_levels(store, &cache, tree)?;
    // Deepest first, so a crash part way never leaves a cached tree whose
    // subtrees are missing, which a later seed would wrongly skip.
    for level in levels.iter().rev() {
        parallel::for_each::<_, SnapshotError, _>(level, |body| {
            cache.put_raw(ObjectKind::Tree, body)?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Reads the trees under `tree` that `cache` lacks, one depth at a time, and
/// returns their bodies grouped by depth, `tree` itself first.
fn read_uncached_levels(
    store: &dyn ObjectStore,
    cache: &LooseObjectStore,
    tree: ObjectId,
) -> Result<Vec<Vec<Vec<u8>>>, SnapshotError> {
    let mut levels = Vec::new();
    let mut depth = vec![tree];
    while !depth.is_empty() {
        let bodies = Mutex::new(Vec::with_capacity(depth.len()));
        let next = Mutex::new(Vec::new());
        parallel::for_each::<_, SnapshotError, _>(&depth, |id| {
            let (body, children) = read_tree(store, id)?;
            let mut uncached = Vec::with_capacity(children.len());
            for child in children {
                if !cache.contains(&child)? {
                    uncached.push(child);
                }
            }
            next.lock().unwrap_or_else(|p| p.into_inner()).extend(uncached);
            bodies.lock().unwrap_or_else(|p| p.into_inner()).push(body);
            Ok(())
        })?;
        levels.push(bodies.into_inner().unwrap_or_else(|p| p.into_inner()));
        depth = next.into_inner().unwrap_or_else(|p| p.into_inner());
        // Identical directories share one tree; read each once per depth.
        depth.sort_unstable();
        depth.dedup();
    }
    Ok(levels)
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
