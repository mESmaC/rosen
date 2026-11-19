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
