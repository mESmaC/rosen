//! Local workspace materialisation (SPEC.md R3): writing a tree's files into
//! a new directory on disk, from the machine's local object store.
//!
//! - With a **source workspace** on a volume that block clones (ReFS, such as
//!   a Dev Drive), every file of the source is block cloned, ignored build
//!   output included, and only the files whose content differs from the new
//!   tree are rewritten. This is the fast path.
//! - Otherwise every tracked file is written from the store by a pool of
//!   threads: the parallel plain copy.
//! - **Hardlinks are never used.** Agents' tools write files in place, and a
//!   hardlinked file would carry that write into the store and every other
//!   workspace sharing it.
//! - A [`Sparse`] selection holds only chosen paths.
//!
//! A workspace is built in a hidden sibling directory and renamed into place
//! when complete, so a crash never leaves a half-written workspace at `dest`.

mod client;
mod clone;
pub(crate) mod delete;
mod layout;
mod lifecycle;
pub(crate) mod rebase;
mod registry;
pub use lifecycle::{deletion_permitted, CleanupReport};
pub use registry::WorkspaceRecord;
pub mod limits;
pub(crate) mod parallel;
pub mod pool;
mod pool_worker;
mod prepare;
mod sparse;
mod staging_sweep;
mod template;
mod write;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rosen_core::ObjectId;
use rosen_store::{ObjectStore, StoreError};

pub use layout::FileKind;
use layout::{disk_path, FileEntry, Layout};
pub use sparse::Sparse;

/// What to materialise besides the tree itself.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub sparse: Sparse,
    /// A workspace already on disk to block clone from, such as the warm
    /// pool's prepared template. Ignored when its volume cannot block clone.
    pub source: Option<SourceWorkspace>,
}

/// A materialised workspace and the tree its tracked files hold. Rosen owns
/// it and nothing writes to its tracked files, so they match `tree`.
#[derive(Debug, Clone)]
pub struct SourceWorkspace {
    pub dir: PathBuf,
    pub tree: ObjectId,
}

/// How a workspace's files were made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Atomically renamed from the daemon's ready pool.
    WarmPool,
    /// Block cloned from a source workspace, then brought to the tree.
    BlockClone,
    /// Every tracked file written from the object store.
    PlainCopy,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::WarmPool => "warm-pool",
            Method::BlockClone => "block-clone",
            Method::PlainCopy => "plain-copy",
        }
    }
}

/// What a materialisation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub method: Method,
    /// Tracked files the workspace holds.
    pub files: u64,
    /// Files block cloned from the source, untracked ones included.
    pub cloned: u64,
    /// Source files plain-copied because the file system refused a clone.
    pub copied: u64,
    /// Tracked files written from the store.
    pub written: u64,
    pub bytes_written: u64,
    /// Why a source workspace was given but not cloned from, if it was not.
    pub fallback: Option<String>,
    /// The project's prepare commands have run in it at its tree (R3), so
    /// an agent's first build compiles only what it changes.
    pub prepared: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MaterialiseError {
    #[error("{0} already exists; a workspace is materialised only into a new directory")]
    Exists(PathBuf),
    #[error("{0} has the name form Rosen reserves for staging; choose another name")]
    ReservedName(PathBuf),
    #[error("{0:?} is not a sparse path: give a path from the workspace root")]
    InvalidSparsePath(String),
    #[error("the tree holds {0:?}, which this OS cannot store as a file name")]
    UnsupportedName(String),
    #[error("block clone of {0} does not hold the source's bytes")]
    CloneMismatch(PathBuf),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

impl MaterialiseError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Materialises `tree` into the new directory `dest`, which must not exist.
pub fn materialise(
    store: &dyn ObjectStore,
    tree: ObjectId,
    dest: &Path,
    options: &Options,
) -> Result<Report, MaterialiseError> {
    materialise_with_budget(store, tree, dest, options, None)
}

/// Materialises with machine-wide admission, used by connected clients and pool workers.
pub fn materialise_with_budget(
    store: &dyn ObjectStore,
    tree: ObjectId,
    dest: &Path,
    options: &Options,
    budget: Option<&limits::Budget>,
) -> Result<Report, MaterialiseError> {
    let target = Layout::of_tree(store, tree, &options.sparse)?;
    let staging = Staging::create(dest)?;
    // Only a budgeted build sweeps: moving abandoned staging must keep its
    // bytes charged to the budget until they are deleted.
    if let Some(budget) = budget {
        staging_sweep::sweep_abandoned(staging.parent(), budget);
    }
    let _permit = reserve(budget, store, &target, options, dest, staging.path())?;
    let mut fallback = None;
    if let Some(source) = &options.source {
        if !clone::may_block_clone(&source.dir, staging.parent()) {
            fallback = Some("the source's volume does not block clone into the destination".into());
        } else {
            match from_source(store, &target, source, staging.path(), &options.sparse) {
                Ok(report) => return staging.publish(dest).map(|()| report),
                Err(error) => fallback = Some(error.to_string()),
            }
            staging.reset()?;
        }
    }
    let mut report = from_store(store, &target, staging.path())?;
    report.fallback = fallback;
    staging.publish(dest)?;
    Ok(report)
}

fn reserve(
    budget: Option<&limits::Budget>,
    store: &dyn ObjectStore,
    target: &Layout,
    options: &Options,
    dest: &Path,
    staging: &Path,
) -> Result<Option<limits::Permit>, MaterialiseError> {
    let Some(budget) = budget else {
        return Ok(None);
    };
    // Each header read waits mostly on opening a file (and, on Windows, on
    // the virus scanner), so threads overlap the waits as the copy does.
    let total = AtomicU64::new(0);
    parallel::for_each::<_, MaterialiseError, _>(&target.files, |entry| {
        let size = store.object_size(&entry.blob)?;
        total.fetch_add(size, Ordering::Relaxed);
        Ok(())
    })?;
    let mut bytes = total.into_inner();
    if let Some(source) = &options.source {
        bytes = bytes.saturating_add(
            limits::disk_bytes(&source.dir)
                .map_err(|error| MaterialiseError::io(&source.dir, error))?,
        );
    }
    budget
        .acquire(dest, staging, bytes)
        .map(Some)
        .map_err(|error| MaterialiseError::io(dest, error))
}
/// The parallel plain copy: every directory, then every file from the store.
fn from_store(
    store: &dyn ObjectStore,
    target: &Layout,
    root: &Path,
) -> Result<Report, MaterialiseError> {
    for dir in &target.dirs {
        let path = disk_path(root, dir);
        fs::create_dir(&path).map_err(|error| MaterialiseError::io(&path, error))?;
    }
    let bytes = write_all(store, root, &target.files.iter().collect::<Vec<_>>())?;
    Ok(Report {
        method: Method::PlainCopy,
        files: target.files.len() as u64,
        cloned: 0,
        copied: 0,
        written: target.files.len() as u64,
        bytes_written: bytes,
        fallback: None,
        prepared: false,
    })
}

fn write_all(
    store: &dyn ObjectStore,
    root: &Path,
    files: &[&FileEntry],
) -> Result<u64, MaterialiseError> {
    let bytes = AtomicU64::new(0);
    parallel::for_each::<_, MaterialiseError, _>(files, |entry| {
        bytes.fetch_add(write::write_entry(store, root, entry)?, Ordering::Relaxed);
        Ok(())
    })?;
    Ok(bytes.into_inner())
}

/// The fast path: block clone the source, then rewrite only the tracked
/// files that differ between its tree and the target.
fn from_source(
    store: &dyn ObjectStore,
    target: &Layout,
    source: &SourceWorkspace,
    root: &Path,
    sparse: &Sparse,
) -> Result<Report, MaterialiseError> {
    let before = Layout::of_tree(store, source.tree, sparse)?;
    let cloned =
        clone::clone_workspace(&source.dir, root, sparse, clone::pick_probe(&before.files))?;
    clone::clear_for_tree(root, &before, target)?;
    let changed = bring_to_tree(root, &before, target)?;
    let bytes = write_all(store, root, &changed)?;
    Ok(Report {
        method: Method::BlockClone,
        files: target.files.len() as u64,
        cloned: cloned.cloned,
        copied: cloned.copied,
        written: changed.len() as u64,
        bytes_written: bytes,
        fallback: None,
        prepared: false,
    })
}

/// Removes the tracked files and directories of `before` that `after` does
/// not hold unchanged, creates `after`'s directories, and returns the files
/// still to write. Untracked files, such as build output, are kept.
fn bring_to_tree<'a>(
    root: &Path,
    before: &Layout,
    after: &'a Layout,
) -> Result<Vec<&'a FileEntry>, MaterialiseError> {
    let old: HashMap<&[u8], &FileEntry> = before
        .files
        .iter()
        .map(|entry| (entry.path.as_slice(), entry))
        .collect();
    let new: HashMap<&[u8], &FileEntry> = after
        .files
        .iter()
        .map(|entry| (entry.path.as_slice(), entry))
        .collect();
    let changed: Vec<&FileEntry> = after
        .files
        .iter()
        .filter(|entry| old.get(entry.path.as_slice()) != Some(entry))
        .collect();
    for entry in &before.files {
        if new.get(entry.path.as_slice()) != Some(&entry) {
            write::remove_file(root, &entry.path)?;
        }
    }
    // Deepest first, so a directory emptied of its children goes too. One
    // that still holds untracked files stays, as git leaves it.
    for dir in before.dirs.iter().rev() {
        let _ = fs::remove_dir(disk_path(root, dir));
    }
    for dir in &after.dirs {
        let path = disk_path(root, dir);
        fs::create_dir_all(&path).map_err(|error| MaterialiseError::io(&path, error))?;
    }
    Ok(changed)
}

/// A hidden directory beside the destination that a workspace is built in.
/// It is removed on drop unless published; one a crash left behind is
/// removed by [`staging_sweep`].
struct Staging {
    parent: PathBuf,
    path: PathBuf,
    published: bool,
}

impl Staging {
    fn create(dest: &Path) -> Result<Self, MaterialiseError> {
        if fs::symlink_metadata(dest).is_ok() {
            return Err(MaterialiseError::Exists(dest.to_path_buf()));
        }
        let name = dest.file_name().ok_or_else(|| {
            MaterialiseError::io(
                dest,
                io::Error::new(io::ErrorKind::InvalidInput, "no directory name"),
            )
        })?;
        // A sweep removes directories of this form once their builder exits,
        // so a workspace must never carry it.
        if staging_sweep::is_staging_name(name) {
            return Err(MaterialiseError::ReservedName(dest.to_path_buf()));
        }
        let parent = std::path::absolute(dest)
            .map_err(|error| MaterialiseError::io(dest, error))?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        fs::create_dir_all(&parent).map_err(|error| MaterialiseError::io(&parent, error))?;
        for attempt in 0u32.. {
            let mut hidden = std::ffi::OsString::from(".");
            hidden.push(name);
            hidden.push(format!(
                "{}{}-{attempt}",
                staging_sweep::STAGING_TAG,
                std::process::id()
            ));
            let path = parent.join(hidden);
            // A crashed run of an earlier process with this id can leave
            // its staging directory behind until a sweep; try the next name.
            match staging_sweep::create_marked(&path) {
                Ok(true) => {
                    return Ok(Self {
                        parent,
                        path,
                        published: false,
                    })
                }
                Ok(false) => continue,
                Err(error) => return Err(MaterialiseError::io(&path, error)),
            }
        }
        unreachable!("the attempt counter is unbounded")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn parent(&self) -> &Path {
        &self.parent
    }

    /// Empties the staging directory for a second attempt.
    fn reset(&self) -> Result<(), MaterialiseError> {
        let io = |error| MaterialiseError::io(&self.path, error);
        fs::remove_dir_all(&self.path).map_err(io)?;
        fs::create_dir(&self.path).map_err(io)
    }

    /// Renames the finished workspace to `dest`. On Windows a virus scanner
    /// or indexer can briefly hold a just-written file open, which blocks
    /// renaming its directory, so the rename is retried for a moment.
    fn publish(mut self, dest: &Path) -> Result<(), MaterialiseError> {
        let mut delay = Duration::from_millis(5);
        loop {
            match fs::rename(&self.path, dest) {
                Ok(()) => {
                    self.published = true;
                    staging_sweep::unmark(&self.path);
                    return Ok(());
                }
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied
                        && delay < Duration::from_secs(1) =>
                {
                    std::thread::sleep(delay);
                    delay *= 2;
                }
                Err(error) => return Err(MaterialiseError::io(dest, error)),
            }
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.published {
            staging_sweep::remove_marked(&self.path);
        }
    }
}
