//! Repository import (SPEC R1): `rosen import <git repo>` loads the history
//! of a git repository's default branch into the object store and names the
//! branch and its tip commit as the project's base.
//!
//! Git itself reads the source repository (`git rev-list` and
//! `git cat-file --batch`), so packfiles, alternates, shallow files and every
//! other on-disk format git supports work without Rosen reimplementing them.
//! Every object is re-hashed by the store on write and must keep its git id,
//! so imported history is bit-identical to the source and later pushes to a
//! git mirror stay fast-forwards.
//!
//! The base is reported only after every reachable object is stored. The
//! caller sets the base ref from the report, so a crash mid-import never
//! leaves a base that points at a missing object, and a rerun skips objects
//! that are already stored.

mod copy;
mod git_source;
pub mod service;

use std::fmt;
use std::path::PathBuf;

use rosen_core::ObjectId;
use rosen_store::ObjectStore;

pub use git_source::GitSource;

/// What to import.
#[derive(Clone, Debug)]
pub struct ImportRequest {
    /// A local repository path (bare or with a working tree) or a URL git
    /// can clone.
    pub source: String,
    /// The branch to make the base. `None` uses the source's default branch,
    /// the one its `HEAD` names.
    pub branch: Option<String>,
}

/// The outcome of a successful import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    /// The branch name the base takes, for example `main`.
    pub branch: String,
    /// The branch's tip commit: the new base commit.
    pub base_commit: ObjectId,
    /// Objects reachable from the base commit.
    pub objects_reachable: usize,
    /// Objects newly written; the rest were already stored.
    pub objects_written: usize,
}

#[derive(Debug)]
pub enum ImportError {
    /// Running git failed, or git reported an error.
    Git { command: String, message: String },
    /// The source uses an object format the store cannot hold.
    UnsupportedObjectFormat(String),
    /// The source's `HEAD` is detached or unborn, so there is no default
    /// branch, and none was named.
    NoDefaultBranch,
    /// The named branch does not exist in the source.
    BranchNotFound(String),
    /// Git's output did not match its documented format.
    Protocol(String),
    /// The store re-hashed an object to a different id than git reported.
    IdMismatch {
        expected: ObjectId,
        stored: ObjectId,
    },
    /// The object store or the metadata store refused a read or write.
    Store(String),
    /// Local file system work, such as the temporary clone, failed.
    Io(std::io::Error),
    /// The import target already has a base. Only a land moves the base
    /// (SPEC §2 Project), so import never replaces one.
    BaseAlreadySet {
        branch: String,
        base_commit: ObjectId,
    },
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git { command, message } => write!(f, "`{command}` failed: {message}"),
            Self::UnsupportedObjectFormat(format) => write!(
                f,
                "the source repository uses the {format} object format; Rosen stores sha1 objects"
            ),
            Self::NoDefaultBranch => write!(
                f,
                "the source repository's HEAD is detached or unborn; name a branch with --branch"
            ),
            Self::BranchNotFound(branch) => {
                write!(f, "branch `{branch}` does not exist in the source repository")
            }
            Self::Protocol(message) => write!(f, "unexpected git output: {message}"),
            Self::IdMismatch { expected, stored } => write!(
                f,
                "object {expected} was stored as {stored}; the source repository may be corrupt"
            ),
            Self::Store(message) => write!(f, "storage error: {message}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::BaseAlreadySet { branch, base_commit } => write!(
                f,
                "this project already has a base ({branch} at {base_commit}); import only into a project with no base"
            ),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<std::io::Error> for ImportError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Imports the history of one branch of a git repository into `store`.
///
/// `scratch_dir` holds a temporary bare clone when `request.source` is a URL;
/// the clone is removed before returning.
pub fn import_repository(
    request: &ImportRequest,
    scratch_dir: PathBuf,
    store: &dyn ObjectStore,
) -> Result<ImportReport, ImportError> {
    let source = GitSource::open(&request.source, scratch_dir)?;
    source.require_sha1()?;
    let branch = match &request.branch {
        Some(branch) => branch.clone(),
        None => source.default_branch()?,
    };
    let base_commit = source.branch_tip(&branch)?;
    let reachable = source.reachable_objects(&base_commit)?;
    let missing = copy::filter_missing(&reachable, store)?;
    copy::copy_objects(&source, &missing, store)?;
    Ok(ImportReport {
        branch,
        base_commit,
        objects_reachable: reachable.len(),
        objects_written: missing.len(),
    })
}
