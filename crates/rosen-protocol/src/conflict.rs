//! Wire shapes for `rosen resolve` (SPEC R5, R10): clearing a change's stored
//! conflicts so the landing gate (R8) lets it land.

use rosen_core::ChangeId;
use serde::{Deserialize, Serialize};

/// Parameters of [`crate::Request::ConflictResolve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveConflicts {
    /// A change ID or unique prefix of one.
    pub change: String,
    /// Repository-relative, `/`-separated files whose open conflicts are
    /// resolved. Each must hold at least one conflict stored on the change.
    pub paths: Vec<String>,
}

/// The answer to [`crate::Request::ConflictResolve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictsResolved {
    pub change: ChangeId,
    /// One entry per requested path, in request order.
    pub resolved: Vec<ResolvedPath>,
    /// Files that still hold open conflicts. Empty means conflicts no longer
    /// stop the change from landing.
    pub open_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPath {
    pub path: String,
    /// Open conflicts this request resolved; 0 when they were already resolved.
    pub conflicts: usize,
}
