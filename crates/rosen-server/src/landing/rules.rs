//! The land decision of SPEC R8 step 2, as a pure function.
//!
//! This is the rule modelled by `proofs/Landing/Design.lean`; the fixture
//! table `proofs/fixtures/landing.tsv` is exported from that model and
//! checked against [`decide`] by `tests/landing_fixtures.rs`.

use rosen_core::{RecheckMode, ReviewMode};

use super::{ChangeState, Tier};

/// How the best stored receipt relates to the change being landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptCheck {
    Missing,
    /// A receipt exists but is for another head or check set, or its base
    /// is not a commit the base ref has ever held.
    Invalid,
    Valid,
    /// A trusted receipt checked this exact merge-result tree, regardless
    /// of the head or base it originally named (R8 step 2).
    SameTree,
}

/// Everything the decision depends on, read in one attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LandInput {
    pub state: ChangeState,
    /// The parent is already landed, or every unlanded ancestor is
    /// eligible to land in this same atomic request (R8 step 3).
    pub parent_landed: bool,
    pub has_conflicts: bool,
    /// An approval exists for the change's current head (R7).
    pub approved: bool,
    /// The project's review setting at the attempt's base (R7): only
    /// `before-land` makes `approved` gate the land.
    pub review: ReviewMode,
    pub recheck: RecheckMode,
    pub tier: Tier,
    pub receipt: ReceiptCheck,
    /// The base has moved since the receipt's base B.
    pub base_moved: bool,
    /// Every change landed since B has disjoint build-unit reach. Vacuously true when the
    /// base has not moved.
    pub landed_since_all_independent: bool,
    /// Merging the change onto the current base is clean.
    pub merge_clean: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    AlreadyLanded,
    Abandoned,
    /// A lower layer cannot land in the atomic stack request (R8 step 3).
    ParentUnlanded,
    Conflicts,
    Unapproved,
    NoReceipt,
    ReceiptStale,
    /// The merge onto the current base conflicts; the change gets stored
    /// conflicts.
    MergeConflict,
    /// Reach mode crossed a dependent land: a new receipt on the merge result is required.
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
    // After-land and prose-only changes need no receipt or re-check (R8).
    let receipt_counts = input.review == ReviewMode::AfterLand
        || input.tier == Tier::ProseOnly
        || input.recheck == RecheckMode::Merge
        || input.receipt == ReceiptCheck::SameTree
        || !input.base_moved
        || input.landed_since_all_independent;
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
    if input.review.approval_gates_land() && !input.approved {
        return Some(Refusal::Unapproved);
    }
    if input.review == ReviewMode::AfterLand || input.tier == Tier::ProseOnly {
        return None;
    }
    match input.receipt {
        ReceiptCheck::Missing => Some(Refusal::NoReceipt),
        ReceiptCheck::Invalid => Some(Refusal::ReceiptStale),
        ReceiptCheck::Valid | ReceiptCheck::SameTree => None,
    }
}

/// True when the decision needs the merge result. Lets the engine skip the
/// expensive merge when the change is refused anyway.
pub fn needs_merge(input: &LandInput) -> bool {
    gate_refusal(input).is_none()
}
