//! The prepared template workspace on a block-cloning volume (SPEC R3).
//!
//! The daemon keeps one template at the base tip and runs the prepare
//! commands only there. Ready workspaces, and a take the pool cannot serve,
//! are block clones of it, ignored build output included, so each starts
//! prepared at almost no copy cost.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rosen_core::ObjectId;
use rosen_protocol::{ErrorBody, ErrorCode, Outcome, PooledWorkspace, Response};
use rosen_store::LooseObjectStore;
use tokio::sync::RwLock;

use super::limits::{Budget, Settings};
use super::pool::{refresh, Prep, Slot};
use super::{materialise_with_budget, MaterialiseError, Method, Options, SourceWorkspace};

/// The template's directory, inside the ready root so a restart removes a
/// template whose prepared state it no longer knows.
pub(super) fn path(pool_root: &Path) -> PathBuf {
    pool_root.join("template")
}

/// Whether files under `pool_root` block clone. Tried with a real clone,
/// because only the file system knows; NTFS and most Linux volumes refuse.
pub(super) fn block_clones(pool_root: &Path) -> bool {
    let (source, clone) = (
        pool_root.join(".clone-probe"),
        pool_root.join(".clone-probe-copy"),
    );
    let bytes = b"rosen block clone probe";
    let works = std::fs::create_dir_all(pool_root).is_ok()
        && std::fs::write(&source, bytes).is_ok()
        && reflink_copy::reflink(&source, &clone).is_ok()
        && std::fs::read(&clone).is_ok_and(|read| read == bytes);
    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&clone);
    works
}

/// The template as a clone source, when it holds `tree` and no prepare is
/// writing into it. Also says whether a clone of it starts prepared.
pub(super) fn source(template: &Option<Slot>, tree: ObjectId) -> Option<(SourceWorkspace, bool)> {
    let slot = template.as_ref()?;
    if slot.tree != tree || slot.prep == Prep::Running {
        return None;
    }
    let source = SourceWorkspace {
        dir: slot.path.clone(),
        tree,
    };
    Some((source, slot.prep == Prep::Done))
}

/// Serves a take the ready pool could not: a block clone of the template
/// into `dest`. Answers `None` when there is no usable template, so the
/// client materialises the workspace itself.
pub(super) async fn take_clone(
    template: &Arc<RwLock<Option<Slot>>>,
    pool_root: &Path,
    tree: ObjectId,
    dest: String,
) -> Outcome {
    // While the worker holds the template to rewrite it, a take does not
    // wait: the client makes the workspace itself straight away.
    let Ok(guard) = Arc::clone(template).try_read_owned() else {
        return Outcome::Ok(Response::PoolTaken(None));
    };
    let Some((source, prepared)) = source(&guard, tree) else {
        return Outcome::Ok(Response::PoolTaken(None));
    };
    let data_dir = pool_root
        .parent()
        .expect("pool data directory")
        .to_path_buf();
    let cloned = tokio::task::spawn_blocking(move || {
        // The read guard keeps the worker from rewriting the template
        // until the clone is done.
        let _template = guard;
        clone_into(&data_dir, &source, Path::new(&dest))
    })
    .await;
    match cloned {
        Ok(Ok(report)) => Outcome::Ok(Response::PoolTaken(Some(PooledWorkspace {
            files: report.files,
            prepared: prepared && report.method == Method::BlockClone,
            from_template: report.method == Method::BlockClone,
        }))),
        Ok(Err(MaterialiseError::Exists(path))) => Outcome::Error(ErrorBody::new(
            ErrorCode::InvalidArgument,
            format!("{} already exists", path.display()),
        )),
        Ok(Err(error)) => {
            eprintln!("rosen warm pool: template clone failed: {error}");
            Outcome::Ok(Response::PoolTaken(None))
        }
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

fn clone_into(
    data_dir: &Path,
    source: &SourceWorkspace,
    dest: &Path,
) -> Result<super::Report, MaterialiseError> {
    let objects = data_dir.join("objects");
    let store = LooseObjectStore::open(&objects).map_err(MaterialiseError::Store)?;
    let limits = data_dir.join("workspace-limits");
    let budget = Budget::open(&limits, Settings::default())
        .map_err(|error| MaterialiseError::io(&limits, error))?;
    let options = Options {
        source: Some(source.clone()),
        ..Options::default()
    };
    materialise_with_budget(&store, source.tree, dest, &options, Some(&budget))
}

/// Brings an existing template to `tree` in place, under the caller's write
/// lock. On failure the template is removed, so it is rebuilt from scratch.
pub(super) async fn refresh_in_place(
    store: &LooseObjectStore,
    template: &mut Option<Slot>,
    tree: ObjectId,
) -> Result<(), crate::Error> {
    let Some(mut slot) = template.take() else {
        return Ok(());
    };
    let store = store.clone();
    let kept = tokio::task::spawn_blocking(move || match refresh(&store, &mut slot, tree) {
        Ok(()) => Some(slot),
        Err(error) => {
            eprintln!("rosen warm pool: template refresh failed: {error}");
            let _ = std::fs::remove_dir_all(&slot.path);
            None
        }
    })
    .await
    .map_err(|error| crate::Error::Unexpected(error.to_string()))?;
    *template = kept;
    Ok(())
}
