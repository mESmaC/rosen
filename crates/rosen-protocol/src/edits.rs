//! Live edit regions from the latest durable snapshot (SPEC R6).
use rosen_core::{ChangeId, ObjectId};
use serde::{Deserialize, Serialize};

/// A view tied to one captured change head and parent tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditRegions {
    pub change: ChangeId,
    pub head: ObjectId,
    pub parent_tree: ObjectId,
    /// All changed paths, sorted. Generated paths are included here; the
    /// overlap query applies the project's exclusions separately.
    pub files: Vec<PathEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathEdit {
    pub path: String,
    pub regions: Regions,
}

/// Added, removed, binary, symlink and mode-changed paths are whole-file edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "spans", rename_all = "snake_case")]
pub enum Regions {
    Whole,
    Lines(Vec<EditSpan>),
}

/// Zero-based half-open parent lines. An insertion has `start == end`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditSpan {
    pub start: usize,
    pub end: usize,
}
