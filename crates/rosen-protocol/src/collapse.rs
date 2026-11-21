//! Health of the Collapse state feed, ledger push and action relay (R9, R10,
//! #434). Times are Unix milliseconds.

use serde::{Deserialize, Serialize};

/// Health of the server's three Collapse links, reported in [`crate::ServerStatus`]
/// when the base's `rosen.toml` names a `collapse` root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollapseHealth {
    /// The Collapse root URL from the base's `rosen.toml`.
    pub root: String,
    pub state_feed: FeedHealth,
    pub ledger_push: FeedHealth,
    pub relay: FeedHealth,
}

/// Current state of one Collapse link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedState {
    /// A request is in flight or unsent work is waiting to go.
    Sending,
    /// Caught up with the endpoint.
    Current,
    /// The last attempt failed and is backing off to retry.
    Failing,
    /// The endpoint answered HTTP 410 (archived or deleted) and sending stopped.
    Stopped,
    /// This server is not the mirror owner (R1, #433), so nothing is sent.
    NotOwner,
    /// Git's credential helper holds no push token for the HTTPS endpoint.
    NoCredential,
}

impl FeedState {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedState::Sending => "sending",
            FeedState::Current => "current",
            FeedState::Failing => "failing",
            FeedState::Stopped => "stopped",
            FeedState::NotOwner => "not_owner",
            FeedState::NoCredential => "no_credential",
        }
    }
}

/// The most recent failure of a Collapse link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedError {
    /// HTTP status the endpoint answered, when it answered.
    #[serde(default)]
    pub http_status: Option<u16>,
    pub message: String,
    /// Unix milliseconds when the failure happened.
    pub at: i64,
}

/// One document or entry the endpoint refused (HTTP 400, 413 or 422) and the
/// feed skipped so the rest of the batch could be delivered (#444).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedItem {
    /// Document name (e.g. `change:<id>`, `config`, `log:<seq>`) or ledger
    /// entry key (e.g. `discovery:42`).
    pub document: String,
    /// HTTP status the endpoint answered (400, 413 or 422).
    pub http_status: u16,
    pub message: String,
    /// Unix milliseconds when the refusal was recorded.
    pub at: i64,
}

/// Delivered sequence in each AIRC ledger, for `ledger_push`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerCursors {
    pub discovery: u64,
    pub past_work: u64,
}

/// Health of one Collapse link (`state_feed`, `ledger_push` or `relay`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedHealth {
    pub state: FeedState,
    /// Unix milliseconds of the most recent delivered batch (or successful
    /// relay exchange), if any.
    #[serde(default)]
    pub last_success_at: Option<i64>,
    /// The last sent sequence or cursor: delivered event sequence for
    /// `state_feed`, highest delivered ledger sequence for `ledger_push`, or
    /// highest action ID taken up for `relay`.
    #[serde(default)]
    pub cursor: Option<u64>,
    /// Per-ledger delivered sequences for `ledger_push`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_cursors: Option<LedgerCursors>,
    /// Doorbell generation last observed by `relay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doorbell_generation: Option<i64>,
    /// Events, ledger entries or unposted relay outcomes not yet sent.
    pub backlog: u64,
    /// The most recent failure; cleared once a subsequent exchange succeeds.
    #[serde(default)]
    pub last_error: Option<FeedError>,
    /// Unix milliseconds when the next retry is scheduled while failing.
    #[serde(default)]
    pub next_retry_at: Option<i64>,
    /// Documents or entries the endpoint refused (HTTP 400, 413 or 422) and
    /// that are skipped until their content changes (#444).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<RefusedItem>,
}
