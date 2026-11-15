//! Repair negotiation for objects that an incremental client will not resend.

use super::SnapshotError;
use rosen_core::{FileMode, ObjectId};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::collections::HashSet;

/// A tree's historical completeness says nothing about subsequent disk damage.
/// Check even a repeated root, before acknowledging it, and return a bounded
/// frontier so a client can repair trees before we inspect their children.
pub(super) fn check(store: &LooseObjectStore, root: ObjectId) -> Result<(), SnapshotError> {
    let mut pending = vec![(root, true)];
    let mut seen = HashSet::new();
    let mut needed = Vec::new();
    while let Some((id, is_tree)) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if id == super::empty_tree() && !store.contains(&id)? {
            continue;
        }
        // Stream large blobs: integrity negotiation must not hold them whole.
        if !store.contains_valid(&id)? {
            needed.push(id);
            if needed.len() == 1024 {
                break;
            }
        } else if is_tree {
            pending.extend(
                store
                    .read_tree(&id)?
                    .entries()
                    .iter()
                    .filter(|entry| entry.mode != FileMode::Submodule)
                    .map(|entry| (entry.id, entry.mode.is_tree())),
            );
        }
    }
    if needed.is_empty() {
        Ok(())
    } else {
        Err(SnapshotError::ObjectsNeeded(needed))
    }
}
