//! Wire shapes for `rosen change new|list|show|abandon` (SPEC R2, R10).

use rosen_core::{AgentId, Change, ChangeState};
use serde::{Deserialize, Serialize};

/// Parameters of [`crate::Request::ChangeNew`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewChange {
    /// The agent that will own the change.
    pub owner: AgentId,
    /// A change ID or unique prefix to stack on. `None` starts the change at
    /// the current tip of the base.
    pub on: Option<String>,
}

/// Parameters of [`crate::Request::ChangeList`]. Every filter is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeFilter {
    pub owner: Option<AgentId>,
    pub state: Option<ChangeState>,
}

/// A change as the server stores it, with its bookkeeping times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    #[serde(flatten)]
    pub change: Change,
    /// Unix milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
}
