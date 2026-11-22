//! Server timing persistence and stats queries (SPEC R8, R9, R10, R12, #449).
//!
//! Only counts, durations, timestamps and compact command labels are stored or
//! returned: never command output, file paths or credentials.

use rosen_core::{ChangeState, ObjectId};
use serde::{Deserialize, Serialize};

/// Check durations reported by an external checker for a post-land checkpoint
/// or bisection step (R12, #449).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordPostLandTiming {
    pub commit: ObjectId,
    #[serde(default)]
    pub kind: crate::CheckKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bisect_session: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lint_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_ms: Option<u64>,
}

fn default_run_class() -> String {
    "build".to_owned()
}

/// One run-queue execution recorded on the server (counts and durations only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordRunTiming {
    /// Compact command or check label (for example `cargo clippy`, `cargo test`).
    pub label: String,
    /// Run-queue class (`landing`, `build`, `verify`, `prepare`).
    #[serde(default = "default_run_class")]
    pub class: String,
    /// Change ID or unique prefix this run was for, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
    /// Unix milliseconds when the run entered the queue.
    pub queued_at_ms: i64,
    /// Unix milliseconds when the run started executing.
    pub started_at_ms: i64,
    /// Unix milliseconds when the run finished.
    pub finished_at_ms: i64,
    /// Process exit code, or `None` if terminated by signal or unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
    /// True when a prior result was reused instead of executing the command.
    #[serde(default)]
    pub reused: bool,
}

/// Query parameters for [`crate::Request::Stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsQuery {
    /// Include activity at or after this Unix millisecond timestamp.
    /// `None` includes all recorded history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    /// Limit per-change stats to this change ID or unique prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
}

/// Per-change phase totals measured by `landing/timing.rs` across all attempts
/// of a land request (milliseconds).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandPhaseTimings {
    pub attempts: u32,
    pub registry_ms: u64,
    pub engine_ms: u64,
    pub attempts_ms: u64,
    pub facts_ms: u64,
    pub tiers_ms: u64,
    pub merge_ms: u64,
    pub sizes_ms: u64,
    pub receipt_ms: u64,
    pub write_ms: u64,
    pub cas_ms: u64,
    pub write_and_cas_ms: u64,
    pub restack_ms: u64,
    pub decision_and_restack_ms: u64,
    pub answer_ms: u64,
    pub total_ms: u64,
}

/// Lifecycle timestamps (Unix ms) and elapsed stage durations for one change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeTimeline {
    pub created_at_ms: i64,
    pub snapshot_at_ms: Option<i64>,
    pub named_at_ms: Option<i64>,
    pub landed_at_ms: Option<i64>,
    pub green_at_ms: Option<i64>,
    pub reverted_at_ms: Option<i64>,
    pub snapshot_to_named_ms: Option<u64>,
    pub named_to_landed_ms: Option<u64>,
    pub snapshot_to_landed_ms: Option<u64>,
    pub landed_to_green_ms: Option<u64>,
    pub landed_to_reverted_ms: Option<u64>,
}

/// Per-change timeline, rebase/conflict counts, run-queue totals and land phases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeStats {
    pub change: String,
    pub owner: String,
    pub state: ChangeState,
    pub timeline: ChangeTimeline,
    pub rebases: u64,
    pub stored_conflicts: u64,
    pub open_conflicts: u64,
    pub conflict_events: u64,
    pub run_count: u64,
    pub run_wait_ms: u64,
    pub run_duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub land_phases: Option<LandPhaseTimings>,
}

/// Run-queue wait and duration percentiles for one check/command label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckLabelStats {
    pub label: String,
    pub runs: u64,
    pub reused: u64,
    pub failures: u64,
    pub wait_p50_ms: Option<u64>,
    pub wait_p95_ms: Option<u64>,
    pub duration_p50_ms: Option<u64>,
    pub duration_p95_ms: Option<u64>,
}

/// One persisted post-land checkpoint timing record (R12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandCheckpointStats {
    pub kind: crate::CheckKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub commit: ObjectId,
    pub batch_size: u64,
    pub build_ms: Option<u64>,
    pub lint_ms: Option<u64>,
    pub test_ms: Option<u64>,
    pub bisect_steps: u64,
    pub outcome: String,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

/// Window aggregates for Velocity, agents and the Collapse state feed (R9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowStats {
    pub since_ms: Option<i64>,
    pub as_of_ms: i64,
    pub lands: u64,
    pub land_latency_p50_ms: Option<u64>,
    pub land_latency_p95_ms: Option<u64>,
    pub snapshot_to_land_p50_ms: Option<u64>,
    pub snapshot_to_land_p95_ms: Option<u64>,
    pub named_to_land_p50_ms: Option<u64>,
    pub named_to_land_p95_ms: Option<u64>,
    pub queue_wait_by_label: Vec<CheckLabelStats>,
    pub post_land_checks: u64,
    pub post_land_failures: u64,
    pub post_land_reverts: u64,
    pub post_land_failure_rate: f64,
    pub revert_rate: f64,
    pub conflicted_changes: u64,
    pub total_changes: u64,
    pub conflict_rate: f64,
}

/// The answer to [`crate::Request::Stats`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsReport {
    pub window: WindowStats,
    pub changes: Vec<ChangeStats>,
    pub checkpoints: Vec<PostLandCheckpointStats>,
}
