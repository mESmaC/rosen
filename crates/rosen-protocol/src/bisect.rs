//! External checker jobs for a failing post-land interval (SPEC R12).
use rosen_core::{AgentId, ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectJob {
    pub session: i64,
    /// A result is accepted once, only for this exact persisted step.
    pub step: u64,
    pub commit: ObjectId,
    #[serde(default)]
    pub kind: crate::post_land::CheckKind,
    /// Target platform for per-platform bisection; absent for any-platform checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectResult {
    pub job: BisectJob,
    pub passed: bool,
    pub output: String,
    /// The checker's nick; the server appends the connection's machine (R14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitter: Option<AgentId>,
    /// Measured check durations for this bisection step (R12, #464).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durations: Option<crate::CheckDurations>,
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
    #[serde(default)]
    pub kind: crate::post_land::CheckKind,
    /// Target platform for per-platform bisection; absent for any-platform checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub job: Option<BisectJob>,
    /// Present only after two culprit failures and a fresh passing parent.
    pub culprit: Option<BisectCulprit>,
    /// Contradictory confirmation; checker must investigate/retry the batch.
    pub flaky: bool,
    /// Failing test names in this session that differ from every known culprit's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing_tests: Vec<String>,
}

/// Where one bisection step stands. A step is `offered` once its job is the
/// session's current one, and `waiting` while it is still only planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Waiting,
    Offered,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectStep {
    /// The persisted step token, from 0, as [`BisectJob::step`] carries it.
    pub step: u64,
    /// Absent for a planned step whose commit the search has not chosen yet.
    pub commit: Option<ObjectId>,
    pub state: StepState,
    /// `nick@machine` of the accepted result; absent before one is accepted.
    pub submitter: Option<String>,
    /// Unix milliseconds the result was accepted.
    pub at: Option<i64>,
}

/// An unresolved bisection, as `rosen review status` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectSession {
    pub id: i64,
    pub kind: crate::post_land::CheckKind,
    /// Target platform for per-platform bisection; absent for any-platform checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The green or built commit (or recorded culprit) the search starts after.
    pub lower: ObjectId,
    pub failing: ObjectId,
    /// Candidate commits after `lower`, up to and including `failing`.
    pub interval: u64,
    /// The halving steps for the interval plus the two confirmations.
    pub planned_steps: u64,
    pub flaky: bool,
    /// Who reported the failing batch that opened the session, and when.
    pub opened_by: Option<String>,
    pub opened_at: Option<i64>,
    /// Failing test names in this session that differ from every known culprit's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing_tests: Vec<String>,
    pub steps: Vec<BisectStep>,
}
