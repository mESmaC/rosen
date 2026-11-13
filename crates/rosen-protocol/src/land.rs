//! Wire shapes for `rosen land` (SPEC R8 step 2, R10).

use rosen_core::{ChangeId, ObjectId};
use serde::{Deserialize, Serialize};

/// The answer to [`crate::Request::Land`]. A refusal is an ordinary answer,
/// not an error: it names what the change must fix before it can land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandAnswer {
    pub change: ChangeId,
    /// The base ref, for example `main`.
    pub base_ref: String,
    /// The base ref's commit after the attempt: the landed commit when the
    /// change landed, otherwise the commit a new receipt must be bound to.
    pub base: ObjectId,
    pub outcome: LandStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LandStatus {
    /// This request moved the base to `commit`. `attempts` counts the
    /// compare-and-swap attempts, more than one when other lands won races.
    Landed { commit: ObjectId, attempts: u32 },
    /// The change had already landed as `commit`; nothing moved. A land is
    /// never applied twice (R8).
    AlreadyLanded { commit: ObjectId },
    /// The land was refused and the base did not move.
    Refused {
        /// Stable code agents branch on: `already_landed`, `abandoned`,
        /// `parent_unlanded`, `conflicts`, `unapproved`, `no_receipt`,
        /// `receipt_stale`, `merge_conflict` or `needs_recheck`.
        reason: String,
        /// What to do next, for a human or an agent reading `--help` output.
        message: String,
        /// Files whose merge onto the base conflicted, for `merge_conflict`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
}
