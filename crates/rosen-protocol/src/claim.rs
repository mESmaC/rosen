//! Wire shapes for `rosen claim|share|wait|release` (SPEC R6, R10). These are
//! also AIRC's file claims, shares and waits (R13): one record per path.

use rosen_core::{AgentId, ChangeId};
use serde::{Deserialize, Serialize};

/// Parameters of the claim, share, wait and release requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// A change ID or unique prefix of one.
    pub change: String,
    /// Repository-relative paths or globs, as typed. The server validates and
    /// normalises them. An empty list in a release means every path.
    pub paths: Vec<String>,
}

/// Parameters of [`crate::Request::ClaimList`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimFilter {
    /// Only this change's records; `None` lists every live change's.
    pub change: Option<String>,
}

/// The three records a change can hold on a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    /// The change owns the path: no other change can claim an overlapping one.
    Claim,
    /// The change edits a path another change claimed, knowingly.
    Share,
    /// The change holds off until the claims on the path are released.
    Wait,
}

/// One stored record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathClaim {
    pub change: ChangeId,
    pub owner: AgentId,
    /// The path or glob, normalised.
    pub path: String,
    pub kind: ClaimKind,
    /// Unix milliseconds when it was recorded.
    pub since: i64,
}

/// A claim on another change that overlaps a requested path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimHolder {
    /// The requested path it blocks.
    pub requested: String,
    /// The holder's claim, which may be a glob that covers `requested`.
    pub held: PathClaim,
}

/// The answer to [`crate::Request::ClaimPaths`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClaimOutcome {
    /// Every requested path is now claimed by the change.
    Granted {
        change: ChangeId,
        paths: Vec<String>,
    },
    /// Nothing was claimed: each holder already claims an overlapping path.
    Refused { holders: Vec<ClaimHolder> },
}

/// The answer to [`crate::Request::SharePaths`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareOutcome {
    pub change: ChangeId,
    pub paths: Vec<String>,
    /// The claims the shared paths overlap; empty when nobody claims them.
    pub holders: Vec<ClaimHolder>,
}

/// The answer to [`crate::Request::WaitPaths`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitOutcome {
    pub change: ChangeId,
    /// The claims being waited on. Each blocked path is woken by a `released`
    /// event for the change once no claim on another change overlaps it.
    pub waiting_on: Vec<ClaimHolder>,
    /// Requested paths that nobody claims: there is nothing to wait for.
    pub free: Vec<String>,
    /// Subscribe from this sequence number to see every wake for this wait.
    pub cursor: u64,
}

/// A wait that a release ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WokenWait {
    pub change: ChangeId,
    pub owner: AgentId,
    pub path: String,
}

/// The answer to [`crate::Request::ReleasePaths`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseOutcome {
    pub change: ChangeId,
    /// The records removed: claims, shares and waits.
    pub released: Vec<PathClaim>,
    /// Waits on other changes that the release unblocked.
    pub woken: Vec<WokenWait>,
}
