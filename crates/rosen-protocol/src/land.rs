//! Wire shapes for `rosen land` (SPEC R8 step 2, R10).

use std::collections::BTreeMap;

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
    Landed {
        commit: ObjectId,
        attempts: u32,
        /// Files the land wrote in other line endings than the change gave
        /// them, following `.gitattributes` or the base (R5, #424).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        line_endings: Vec<LineEndingRewrite>,
    },
    /// The change had already landed as `commit`; nothing moved. A land is
    /// never applied twice (R8).
    AlreadyLanded { commit: ObjectId },
    /// The land was refused and the base did not move.
    Refused {
        /// Stable code agents branch on: `already_landed`, `abandoned`,
        /// `parent_unlanded`, `conflicts`, `unapproved`, `no_receipt`,
        /// `receipt_stale`, `merge_conflict`, `regeneration_required`,
        /// `regeneration_failed`, `oversize_files` or `needs_recheck`.
        reason: String,
        /// What to do next, for a human or an agent reading `--help` output.
        message: String,
        /// Files whose merge conflicted, need regeneration, or exceed the land
        /// limit. For `oversize_files`, the message also gives each size in bytes.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
        /// The merged input tree awaiting regeneration, for
        /// `regeneration_required` and `regeneration_failed` (#428).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merged_tree: Option<ObjectId>,
        /// Commands registered in `rosen.toml` `[generated.commands]` for
        /// `paths`, when configured (#428).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        commands: BTreeMap<String, String>,
        /// Command output log when `reason` is `regeneration_failed` (#428).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log: Option<String>,
    },
}

/// The answer to [`crate::Request::LandCheck`] (R8 "Land check"): every gate
/// a land of the change would apply on the current base, read without
/// landing. Nothing changes: the change keeps its state, and no event or
/// receipt is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandCheck {
    pub change: ChangeId,
    pub base_ref: String,
    /// The base commit every verdict was read against.
    pub base: ObjectId,
    pub head: ObjectId,
    /// The project's `review` setting at the base: `before-land` or `after-land`.
    pub review_mode: String,
    /// The project's `recheck` setting at the base: `reach` or `merge`.
    pub recheck_mode: String,
    /// The review tier the land would record: `prose`, `normal` or `sensitive`.
    pub tier: String,
    pub reach: LandReach,
    pub merge: MergeGate,
    /// Files a clean merge result holds in other line endings than the
    /// change gave them, following `.gitattributes` or the base (R5, #424).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub line_endings: Vec<LineEndingRewrite>,
    pub approval: ApprovalGate,
    pub receipt: ReceiptGate,
    /// The unlanded ancestors an atomic land would include, bottom first.
    pub stack: Vec<StackLayer>,
    /// True when a land on this base would commit, barring a lost
    /// compare-and-swap.
    pub landable: bool,
    /// The refusal a land on this base would answer with, in the land's own
    /// order. Absent exactly when `landable` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<LandRefusal>,
    /// The commit the change already landed as, when it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed: Option<ObjectId>,
}

/// A file whose line endings a land rewrote from what the change gave it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineEndingRewrite {
    pub path: String,
    /// The newline the file landed with: `lf` or `crlf`.
    pub eol: String,
}

/// The same reason, message and paths [`LandStatus::Refused`] would carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandRefusal {
    pub reason: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_tree: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

/// The build units a change's edits reach (SPEC §2 Reach), or `"all"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LandReach {
    Units(Vec<String>),
    All(ReachAll),
}

/// Serializes as the string `"all"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachAll {
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MergeGate {
    /// The change merges onto the base cleanly.
    Clean,
    /// The change holds stored conflicts (R5); it is not merged until they
    /// are resolved.
    Conflicts { conflicts: Vec<StoredConflict> },
    /// Merging `change`, the top or one of its ancestors, onto the base
    /// conflicts in `paths`. A land would store these conflicts.
    MergeConflict {
        change: ChangeId,
        paths: Vec<String>,
    },
    /// Generated files of `change` must be regenerated on the base (§3, #428).
    RegenerationRequired {
        change: ChangeId,
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merged_tree: Option<ObjectId>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        commands: BTreeMap<String, String>,
    },
    /// Files whose blobs exceed `max_file_size` (R1).
    OversizeFiles {
        limit: u64,
        files: Vec<OversizeFile>,
    },
    /// The merge result still holds conflict markers (R8).
    ConflictMarkers { paths: Vec<String> },
    /// The change has landed or is abandoned, so there is nothing to merge.
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredConflict {
    /// The conflict's ID, as `rosen resolve` and `rosen status` show it.
    pub id: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OversizeFile {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApprovalGate {
    /// Approvals hold for `head`, from these reviewers (`nick@machine`).
    Held {
        reviewers: Vec<rosen_core::AgentId>,
        head: ObjectId,
    },
    /// A `before-land` project needs an approval for the current head.
    Needed,
    /// An `after-land` project lands without an approval (R7).
    NotRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReceiptGate {
    /// A receipt counts on this base: it names this base, or it checked the
    /// exact merge result the land would publish.
    Valid {
        receipt: ReceiptBinding,
    },
    /// The base moved past the receipt's base, and the receipt still counts
    /// across the lands since (reach or merge recheck mode).
    Reusable {
        receipt: ReceiptBinding,
        landed_since: Vec<LandedSince>,
    },
    /// The base moved past the receipt's base with lands whose reach
    /// intersects the change's, so a land needs a new receipt.
    Stale {
        receipt: ReceiptBinding,
        intersecting: Vec<LandedSince>,
    },
    /// Receipts exist, but none is bound to the current head and check set
    /// with a key that still verifies.
    Unbound,
    Missing,
    /// After-land projects and prose-only changes land without a receipt.
    NotRequired,
    /// The merge did not produce a result for a receipt to be matched to.
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptBinding {
    pub check_set: String,
    pub head: ObjectId,
    pub base: ObjectId,
    /// `nick@machine`.
    pub submitter: String,
}

/// A land on the base after a receipt's base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandedSince {
    pub commit: ObjectId,
    /// The landed change; absent for a mirror adoption or revert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeId>,
}

/// One unlanded ancestor an atomic stack land would include.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackLayer {
    pub change: ChangeId,
    pub head: ObjectId,
    pub state: rosen_core::ChangeState,
    pub tier: String,
    /// What stops this layer landing, as a refusal code, when something does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}
