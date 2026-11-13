//! Checks a comment's anchor against the change head it names: the path must
//! be a file in that tree, and the line must exist in the file.

use super::ReviewError;
use rosen_core::{FileMode, ObjectId};
use rosen_store::ObjectStore;

/// Reads `path` from the tree `head` and checks that `line` exists in it.
pub fn check(
    store: &dyn ObjectStore,
    head: ObjectId,
    path: &str,
    line: u32,
) -> Result<(), ReviewError> {
    let blob = file_at(store, head, path)?;
    let data = store.read_blob(&blob).map_err(store_error)?.data;
    rosen_core::review::check_line(&data, line).map_err(ReviewError::from)
}

/// Walks the tree one segment at a time. Only a file, an executable or a
/// symlink (stored as a file holding its target) has lines to comment on.
fn file_at(store: &dyn ObjectStore, head: ObjectId, path: &str) -> Result<ObjectId, ReviewError> {
    let missing = || ReviewError::Invalid(format!("{path} is not a file at head {head}"));
    let mut tree = head;
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        let listing = store.read_tree(&tree).map_err(store_error)?;
        let entry = listing.get(segment.as_bytes()).ok_or_else(missing)?;
        let is_last = segments.peek().is_none();
        match (is_last, entry.mode) {
            (false, mode) if mode.is_tree() => tree = entry.id,
            (true, FileMode::Regular | FileMode::Executable | FileMode::Symlink) => {
                return Ok(entry.id)
            }
            (true, FileMode::Legacy(_)) if !entry.mode.is_tree() => return Ok(entry.id),
            _ => return Err(missing()),
        }
    }
    Err(missing())
}

fn store_error(err: rosen_store::StoreError) -> ReviewError {
    ReviewError::Store(err.to_string())
}
