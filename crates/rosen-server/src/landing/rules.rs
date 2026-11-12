//! The land decision of SPEC R8 step 2, as a pure function.
//!
//! This is the rule modelled by `proofs/Landing/Design.lean`; the fixture
//! table `proofs/fixtures/landing.tsv` is exported from that model and
//! checked against [`decide`] by `tests/landing_fixtures.rs`.

use super::{ChangeState, Tier};

/// How the best stored receipt relates to the change being landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptCheck {
    Missing,
    /// A receipt exists but is for another head or check set, or its base
    /// is not a commit the base ref has ever held.
    Invalid,
    Valid,
}

/// Everything the decision depends on, read in one attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LandInput {
    pub state: ChangeState,
    /// The parent is a base commit, or a change that has already landed.
    pub parent_landed: bool,
    pub has_conflicts: bool,
    /// An approval exists for the change's current head (R7).
    pub approved: bool,
    pub tier: Tier,
    pub receipt: ReceiptCheck,
    /// The base has moved since the receipt's base B.
    pub base_moved: bool,
    /// Every change landed since B is prose-only. Vacuously true when the
    /// base has not moved.
    pub landed_since_all_prose: bool,
    /// Merging the change onto the current base is clean.
    pub merge_clean: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    AlreadyLanded,
    Abandoned,
    /// A stack lands bottom first (R8 step 3).
    ParentUnlanded,
    Conflicts,
    Unapproved,
    NoReceipt,
    ReceiptStale,
    /// The merge onto the current base conflicts; the change gets stored
    /// conflicts.
    MergeConflict,
    /// The base moved past a non-prose land: a new receipt on the new merge
    /// result is required.
    NeedsRecheck,
}

impl Refusal {
    /// Stable code used in the fixture table and in `--json` output.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::AlreadyLanded => "already_landed",
            Refusal::Abandoned => "abandoned",
            Refusal::ParentUnlanded => "parent_unlanded",
            Refusal::Conflicts => "conflicts",
            Refusal::Unapproved => "unapproved",
            Refusal::NoReceipt => "no_receipt",
            Refusal::ReceiptStale => "receipt_stale",
            Refusal::MergeConflict => "merge_conflict",
            Refusal::NeedsRecheck => "needs_recheck",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Land,
    Refuse(Refusal),
}

impl Verdict {
    pub fn code(self) -> &'static str {
        match self {
            Verdict::Land => "land",
            Verdict::Refuse(refusal) => refusal.code(),
        }
    }
}

/// Decides one land attempt. Checks run in a fixed order so that the
/// refusal an agent sees names the first thing it must fix.
pub fn decide(input: &LandInput) -> Verdict {
    if let Some(refusal) = gate_refusal(input) {
        return Verdict::Refuse(refusal);
    }
    if !input.merge_clean {
        return Verdict::Refuse(Refusal::MergeConflict);
    }
    // A prose-only change needs no receipt, so no re-check either (R8 step 1).
    let receipt_counts =
        input.tier == Tier::ProseOnly || !input.base_moved || input.landed_since_all_prose;
    if receipt_counts {
        Verdict::Land
    } else {
        Verdict::Refuse(Refusal::NeedsRecheck)
    }
}

/// The checks that do not depend on the merge result.
fn gate_refusal(input: &LandInput) -> Option<Refusal> {
    match input.state {
        ChangeState::Landed => return Some(Refusal::AlreadyLanded),
        ChangeState::Abandoned => return Some(Refusal::Abandoned),
        ChangeState::Open | ChangeState::Ready => {}
    }
    if !input.parent_landed {
        return Some(Refusal::ParentUnlanded);
    }
    if input.has_conflicts {
        return Some(Refusal::Conflicts);
    }
    if !input.approved {
        return Some(Refusal::Unapproved);
    }
    if input.tier == Tier::ProseOnly {
        return None;
    }
    match input.receipt {
        ReceiptCheck::Missing => Some(Refusal::NoReceipt),
        ReceiptCheck::Invalid => Some(Refusal::ReceiptStale),
        ReceiptCheck::Valid => None,
    }
}

/// True when the decision needs the merge result. Lets the engine skip the
/// expensive merge when the change is refused anyway.
pub fn needs_merge(input: &LandInput) -> bool {
    gate_refusal(input).is_none()
}
