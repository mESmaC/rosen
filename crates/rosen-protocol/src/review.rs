//! Wire shapes for `rosen review comment|resolve|list` (SPEC R7, R10).

use rosen_core::{AgentId, ChangeId, ObjectId, ThreadState};
use serde::{Deserialize, Serialize};

/// Parameters of [`crate::Request::ReviewComment`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentRequest {
    /// A change ID or unique prefix of one.
    pub change: String,
    pub author: AgentId,
    pub body: String,
    /// The head tree the commenter read. The server refuses the comment when
    /// the change has a different head now, because its line numbers may no
    /// longer match. `None` anchors to whatever head the change has.
    #[serde(default)]
    pub head: Option<ObjectId>,
    pub target: CommentTarget,
}

/// Where a comment goes: a new thread on a line, or a reply in a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum CommentTarget {
    /// Starts a thread on `line` (1-based) of `path` at the change's head.
    Line { path: String, line: u32 },
    /// Replies in a thread on the same change. A reply reopens a resolved
    /// thread.
    Thread { thread: u64 },
}

/// Parameters of [`crate::Request::ReviewResolve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveRequest {
    pub thread: u64,
    /// Who resolved it, recorded on the thread.
    pub by: AgentId,
}

/// Parameters of [`crate::Request::ReviewList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadFilter {
    /// A change ID or unique prefix of one.
    pub change: String,
    /// Only threads in this state; `None` lists every thread.
    #[serde(default)]
    pub state: Option<ThreadState>,
}

/// One comment in a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub author: AgentId,
    pub body: String,
    /// The change's head tree when the comment was written.
    pub head: ObjectId,
    /// Unix milliseconds.
    pub created_at: i64,
}

/// A thread with its comments, oldest first. `path`, `line` and `head` are
/// the anchor its first comment set; replies never move it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    pub id: u64,
    pub change: ChangeId,
    pub path: String,
    pub line: u32,
    pub head: ObjectId,
    pub state: ThreadState,
    pub opened_by: AgentId,
    /// Who resolved it; set exactly when `state` is `resolved`.
    pub resolved_by: Option<AgentId>,
    pub created_at: i64,
    pub updated_at: i64,
    pub comments: Vec<ReviewComment>,
}

/// Approve the exact head the reviewer read; stale approvals are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveRequest {
    pub change: String,
    pub reviewer: AgentId,
    pub head: ObjectId,
}

/// An approval retains its original reviewed head after a server rebase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub id: u64,
    pub change: ChangeId,
    pub reviewer: AgentId,
    /// The machine the approval came from (R14), taken from the connection:
    /// an enrolled machine's id, or `server`. Absent on approvals recorded
    /// before it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    pub head: ObjectId,
    pub created_at: i64,
    pub valid: bool,
}
