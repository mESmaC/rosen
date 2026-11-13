//! Review tiers from immutable diffs and the configuration in force on base (R7).

use crate::history::trees::{changed_files, Change as FileChange};
use crate::landing::Tier;
use crate::ServerState;
use rosen_core::{Change, ChangeParent, FileMode, Object, ObjectId, ProjectConfig};
use rosen_store::{LooseObjectStore, ObjectStore};

/// Computes a live change's tier. Its stack parent defines its own edits;
/// the current project base defines policy, even when the parent is older.
pub fn for_change(state: &ServerState, change: &Change) -> Result<Tier, String> {
    let (_, resolved) = state
        .project_config
        .at_base()?
        .ok_or_else(|| "the project has no base yet".to_owned())?;
    let parent = match change.parent {
        ChangeParent::Base(commit) => commit,
        ChangeParent::Change(id) => {
            state
                .changes
                .show(&id.to_string())
                .map_err(message)?
                .change
                .head
        }
    };
    let store = LooseObjectStore::open(state.config.objects_dir()).map_err(message)?;
    classify(&store, &resolved.config, &parent, &change.head)
}

/// Computes against an explicit base so landing retries can bind policy to
/// the same base as their compare-and-swap. Parent and head accept trees or commits.
pub fn compute(
    store: &dyn ObjectStore,
    config_base: &ObjectId,
    parent: &ObjectId,
    head: &ObjectId,
) -> Result<Tier, String> {
    let resolved = crate::project_config::resolve(store, config_base).map_err(message)?;
    classify(store, &resolved.config, parent, head)
}

fn classify(
    store: &dyn ObjectStore,
    config: &ProjectConfig,
    parent: &ObjectId,
    head: &ObjectId,
) -> Result<Tier, String> {
    let old = tree(store, parent)?;
    let new = tree(store, head)?;
    let files = changed_files(store, Some(old), Some(new), &[]).map_err(message)?;
    // Check all paths first: even a prose file takes sensitive precedence.
    if files.iter().any(|file| config.is_sensitive(&file.path)) {
        return Ok(Tier::Sensitive);
    }
    for file in &files {
        if !prose(store, config, file)? {
            return Ok(Tier::Normal);
        }
    }
    Ok(Tier::ProseOnly)
}

fn tree(store: &dyn ObjectStore, id: &ObjectId) -> Result<ObjectId, String> {
    match store.read(id).map_err(message)? {
        Object::Commit(commit) => Ok(commit.tree),
        Object::Tree(_) => Ok(*id),
        _ => Err("review tier expects a commit or tree".to_owned()),
    }
}

fn prose(
    store: &dyn ObjectStore,
    config: &ProjectConfig,
    file: &FileChange,
) -> Result<bool, String> {
    // Build inputs and generated/registration files may affect execution even
    // with a documentation extension; they must retain the receipt gate.
    if config.is_generated(&file.path)
        || config.is_registration_list(&file.path)
        || config
            .workspace_inputs
            .iter()
            .any(|p| p.matches(&file.path))
    {
        return Ok(false);
    }
    let extension = file.path.rsplit_once('.').map(|(_, ext)| ext);
    if !matches!(extension, Some("md" | "markdown" | "txt" | "rst")) {
        return Ok(false);
    }
    for entry in [file.old, file.new].into_iter().flatten() {
        if entry.mode != FileMode::Regular {
            return Ok(false);
        }
        let blob = store.read_blob(&entry.id).map_err(message)?;
        if blob.data.contains(&0) || std::str::from_utf8(&blob.data).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn message(error: impl std::fmt::Display) -> String {
    error.to_string()
}
