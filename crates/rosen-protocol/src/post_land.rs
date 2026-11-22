//! External build and full-test checker interface (SPEC R12).
use rosen_core::{project_config::CheckSet, AgentId, ObjectId};
use serde::{Deserialize, Serialize};

/// Build includes lint; tests may fail without making an after-land base unbuildable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckKind {
    Build,
    #[default]
    Test,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Test => "test",
        }
    }
}

/// Scheduling contract for external checkers: build repairs precede full tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckPriority {
    Landing,
    #[default]
    Verify,
}

impl CheckPriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Landing => "landing",
            Self::Verify => "verify",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandJob {
    pub commit: ObjectId,
    #[serde(default)]
    pub kind: CheckKind,
    /// Run-queue class for the external checker (`rosen run --class`).
    #[serde(default)]
    pub priority: CheckPriority,
    /// Configuration in force at this landed commit, including commands.
    pub check_set: CheckSet,
    /// Required target platform this job checks (from `check_set.platforms`).
    /// Absent when the check set runs on any platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The checker's nick, set when it reports this job's result. The server
    /// records it with the machine from the connection, never from the
    /// request (R14), so it may not hold `@`. An offered job carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitter: Option<AgentId>,
}

/// A confirmed failure stays attached to its exact land even after repair or revert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandFailure {
    pub session: i64,
    pub change: String,
    pub commit: ObjectId,
    pub kind: CheckKind,
    /// Platform on which the failure was confirmed; absent for any-platform checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub output: String,
    /// Failing test names parsed from this failure that differ from earlier known culprits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing_tests: Vec<String>,
    pub requires_revert: bool,
    pub revert: Option<ObjectId>,
    /// `nick@machine` of the result that confirmed the culprit, and when, in
    /// Unix milliseconds. Absent for failures confirmed before it was recorded.
    #[serde(default)]
    pub submitter: Option<String>,
    #[serde(default)]
    pub submitted_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandWork {
    /// Newest landed commit beyond the green prefix; repeated offers are safe.
    pub job: Option<PostLandJob>,
    /// No commit is assumed green before the first successful full run.
    pub last_green: Option<ObjectId>,
}

/// A verified checkpoint: the commit, who reported the passing run and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub commit: ObjectId,
    /// `nick@machine`; absent for results submitted before it was recorded.
    pub submitter: Option<String>,
    /// Unix milliseconds; absent for results submitted before it was recorded.
    pub at: Option<i64>,
}

/// A confirmed culprit whose required inverse has not landed (R12 "Auto-revert").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInverse {
    pub culprit: ObjectId,
    pub change: String,
    pub kind: CheckKind,
    /// Platform on which the failure was confirmed; absent for any-platform checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The base of the latest refused attempt; absent before any refusal.
    pub attempted_base: Option<ObjectId>,
    pub paths: Vec<String>,
    pub overlapping_commits: Vec<ObjectId>,
    pub reason: Option<String>,
    /// Unix milliseconds of the first refusal recorded with a time.
    pub first_refused_at: Option<i64>,
}

/// One live checker connection matching a platform (R12, #440).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectedChecker {
    /// Machine ID (`"server"` on the local transport, or the enrolled machine ID).
    pub machine: String,
    /// Platform reported by the checker (OS and architecture, e.g. `macos-arm64`).
    pub platform: String,
}

/// Per-platform post-land checkpoints and live checker coverage (R12, #440, #457).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformStatus {
    /// Configured platform target (e.g. `"windows"`, `"macos"`, `"linux"`).
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_built: Option<Checkpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_green: Option<Checkpoint>,
    #[serde(default)]
    pub required_build: bool,
    #[serde(default)]
    pub required_test: bool,
    #[serde(default)]
    pub checkers: Vec<ConnectedChecker>,
    /// Unix milliseconds since no matching checker has been online while
    /// checkpoints wait on this required platform (set once waiting >= 1 h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_no_checker_since: Option<i64>,
}

impl PlatformStatus {
    /// True when this platform gates either build or full-test checkpoints.
    pub fn is_required(&self) -> bool {
        self.required_build || self.required_test
    }

    /// Loud warning when this required platform has had no checker online for
    /// an hour while checkpoints wait on it (R12, #457).
    pub fn waiting_no_checker_message(&self) -> Option<String> {
        let since = self.waiting_no_checker_since?;
        let label = rosen_core::Platform::display_label(&self.platform);
        Some(format!(
            "post-land waiting on {label}: no checker online since {}",
            format_utc_minute(since)
        ))
    }
}

/// `YYYY-MM-DD HH:MM UTC` from Unix milliseconds (Howard Hinnant's civil-from-days).
fn format_utc_minute(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        of_day / 3600,
        of_day % 3600 / 60
    )
}

/// The read-only answer to [`crate::Request::PostLandStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandStatus {
    /// The project base tip; absent before the first import.
    pub base: Option<ObjectId>,
    pub last_built: Option<Checkpoint>,
    pub last_green: Option<Checkpoint>,
    /// Per-platform checkpoints and live checker coverage, when configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<PlatformStatus>,
    pub sessions: Vec<crate::BisectSession>,
    pub pending_inverses: Vec<PendingInverse>,
}
