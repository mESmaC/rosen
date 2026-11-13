//! External checker jobs for a failing post-land interval (SPEC R12).
use rosen_core::ObjectId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectJob {
    pub session: i64,
    /// A result is accepted once, only for this exact persisted step.
    pub step: u64,
    pub commit: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectResult {
    pub job: BisectJob,
    pub passed: bool,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectCulprit {
    pub commit: ObjectId,
    pub parent: ObjectId,
    pub change: String,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectState {
    pub id: i64,
    pub job: Option<BisectJob>,
    /// Present only after two culprit failures and a fresh passing parent.
    pub culprit: Option<BisectCulprit>,
    /// Contradictory confirmation; checker must investigate/retry the batch.
    pub flaky: bool,
}
