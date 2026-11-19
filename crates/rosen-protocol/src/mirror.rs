//! Operator history repair (SPEC R1); no stored object is deleted.
use rosen_core::ObjectId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorAdoption {
    pub base_ref: String,
    pub old_base: ObjectId,
    pub new_base: ObjectId,
    pub remote_tip: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRewrite {
    pub base_ref: String,
    pub old_base: ObjectId,
    pub new_base: ObjectId,
    pub mirror_boundary: Option<ObjectId>,
    pub rewritten_commits: usize,
    pub updated_changes: usize,
    pub paths: Vec<String>,
}

/// What `rosen mirror catch-up` found on the mirror branch (R1, #416).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUpStatus {
    /// The mirror branch is at the base.
    UpToDate,
    /// The base already contains the mirror tip; the next push sends the rest.
    Ahead,
    /// The base moved forward to the mirror tip.
    FastForwarded,
}

/// An open change the catch-up could not rebase; it rebases when it lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchUpRebaseError {
    pub change: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorCatchUp {
    pub base_ref: String,
    pub status: CatchUpStatus,
    pub old_base: ObjectId,
    pub new_base: ObjectId,
    pub remote_tip: ObjectId,
    /// The base's new commits along the mirror's first-parent line, oldest
    /// first. Each is recorded in base history as imported, not as a land.
    pub commits: Vec<ObjectId>,
    /// Open changes moved onto the new base.
    pub rebased: Vec<String>,
    /// The rebased changes that now hold conflicts.
    pub conflicted: Vec<String>,
    pub rebase_errors: Vec<CatchUpRebaseError>,
}
