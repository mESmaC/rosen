//! Post-land deploy step result submission (SPEC R12).
use rosen_core::{AgentId, ObjectId};
use serde::{Deserialize, Serialize};

/// Outcome of a deploy step reported by an external checker (SPEC R12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployStatus {
    Passed,
    Failed,
    Skipped,
}

impl DeployStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Request payload for [`crate::Request::DeployResult`] (SPEC R12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitDeployResult {
    /// The deploy run ID from the `deploy_due` event.
    pub id: i64,
    /// The deploy step name from `rosen.toml`, checked against the run.
    pub step: String,
    /// The green commit from the `deploy_due` event, checked against the run.
    pub commit: ObjectId,
    /// Whether the deploy step passed, failed or was skipped.
    pub status: DeployStatus,
    /// Command output or skip reason, if any.
    #[serde(default)]
    pub output: String,
    /// The checker's nick, without `@`; the server appends its machine (R14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitter: Option<AgentId>,
}

/// A recorded deploy run, returned by [`crate::Request::DeployResult`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployRecord {
    pub id: i64,
    pub step: String,
    pub commit: ObjectId,
    pub from: Option<ObjectId>,
    pub command: String,
    pub status: DeployStatus,
    pub output: String,
    pub submitter: Option<String>,
    pub submitted_at: Option<i64>,
}
