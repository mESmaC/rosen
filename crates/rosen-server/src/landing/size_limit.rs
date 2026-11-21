//! Prevent a land from introducing a blob its mirror cannot carry (R1).
use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use rosen_core::FileMode;
use rosen_protocol::LandStatus;
use rosen_store::ObjectStore;

use super::{LandError, LandResult, TreeId};

type OversizeFiles = Vec<(String, u64)>;
type TreeCache = HashMap<(TreeId, u64), OversizeFiles>;
static CHECKED_TREES: OnceLock<Mutex<TreeCache>> = OnceLock::new();

pub(super) fn check(store: &dyn ObjectStore, trees: &[TreeId], limit: u64) -> LandResult<()> {
    let cache = CHECKED_TREES.get_or_init(Default::default);
    let mut files = BTreeSet::new();
    for tree in trees {
        files.extend(oversize(store, *tree, limit, cache)?);
    }
    if files.is_empty() {
        return Ok(());
    }
    Err(LandError::OversizeFiles {
        limit,
        files: files.into_iter().collect(),
    })
}

/// Immutable subtree IDs let later lands inspect only changed directories.
/// Read blob headers for lengths: never inflate a 198 MB file to refuse it.
fn oversize(
    store: &dyn ObjectStore,
    tree: TreeId,
    limit: u64,
    cache: &Mutex<TreeCache>,
) -> LandResult<OversizeFiles> {
    if let Some(files) = cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(tree, limit))
    {
        return Ok(files.clone());
    }
    let mut files = Vec::new();
    for entry in store.read_tree(&tree).map_err(backend)?.entries() {
        let name = String::from_utf8_lossy(&entry.name);
        if entry.mode.is_tree() {
            for (path, size) in oversize(store, entry.id, limit, cache)? {
                files.push((format!("{name}/{path}"), size));
            }
        } else if entry.mode != FileMode::Submodule {
            let size = store.object_size(&entry.id).map_err(backend)?;
            if size > limit {
                files.push((name.into_owned(), size));
            }
        }
    }
    cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert((tree, limit), files.clone());
    Ok(files)
}

fn backend(error: impl std::fmt::Display) -> LandError {
    LandError::Backend(error.to_string())
}

pub(super) fn message(limit: u64, files: &[(String, u64)]) -> String {
    let names: Vec<_> = files
        .iter()
        .map(|(path, size)| format!("{path:?}: {size} bytes"))
        .collect();
    format!(
        "files exceed max_file_size={limit} bytes: {}. Remove them from the change \
        and ignore build output, or have the operator land a suitable rosen.toml \
        max_file_size before retrying",
        names.join(", ")
    )
}

pub(super) fn status(limit: u64, files: OversizeFiles) -> LandStatus {
    LandStatus::Refused {
        reason: "oversize_files".into(),
        message: message(limit, &files),
        paths: files.into_iter().map(|(path, _)| path).collect(),
        merged_tree: None,
        commands: std::collections::BTreeMap::new(),
        log: None,
    }
}
