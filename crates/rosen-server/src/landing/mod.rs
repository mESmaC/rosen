//! Optimistic landing (SPEC R8, D3, D19).
//!
//! There is no global merge queue or lock. A land re-merges the change onto
//! the current base outside any lock, decides with the pure rules in
//! [`rules`], writes the single landed commit, and then moves the base by a
//! compare-and-swap in one SQLite transaction ([`Ledger`]). A lost race is
//! retried with backoff while no new receipt is needed. After a land the
//! server rebases stacked and overlapping changes ([`restack`]).
//!
//! The change model, merge, object store and event bus are separate pieces
//! of the server. Landing reaches them only through the traits below, so each
//! can be wired in without touching the landing algorithm.

mod engine;
mod ledger;
mod restack;
pub mod rules;

use std::fmt;

pub use engine::{LandOutcome, Landing, LandingConfig, LandingDeps, MergePreview};
pub use ledger::{CasResult, LandRecord, Ledger};
pub use restack::RestackReport;
pub use rules::{LandInput, ReceiptCheck, Refusal, Verdict};

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

id_type!(
    /// A change's stable ID (SPEC §2).
    ChangeId
);
id_type!(
    /// A git-format commit object ID.
    CommitId
);
id_type!(
    /// A git-format tree object ID.
    TreeId
);

/// Review tier computed by the server from a change's diff (SPEC R7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    ProseOnly,
    Normal,
    Sensitive,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::ProseOnly => "prose",
            Tier::Normal => "normal",
            Tier::Sensitive => "sensitive",
        }
    }

    pub fn parse(text: &str) -> Option<Tier> {
        match text {
            "prose" => Some(Tier::ProseOnly),
            "normal" => Some(Tier::Normal),
            "sensitive" => Some(Tier::Sensitive),
            _ => None,
        }
    }
}

/// Change states (SPEC §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeState {
    Open,
    Ready,
    Landed,
    Abandoned,
}

/// What a change sits on: a base commit, or another change (a stack, D19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parent {
    Base(CommitId),
    Change(ChangeId),
}

/// The facts landing reads about one change from the change model.
#[derive(Clone, Debug)]
pub struct ChangeFacts {
    pub id: ChangeId,
    pub owner: String,
    pub state: ChangeState,
    pub parent: Parent,
    /// The change's current head commit.
    pub head: CommitId,
    /// The commit the change's edits are measured against: its base commit,
    /// or its parent change's head when stacked. It is the merge base when
    /// the server merges the change onto a newer base.
    pub merge_base: CommitId,
    pub tier: Tier,
    /// The change still holds stored conflicts (SPEC R5).
    pub has_conflicts: bool,
    /// The head the change's latest approval was given at (SPEC R7).
    pub approved_head: Option<CommitId>,
}

/// A record that the pre-land check set passed on one exact merge result
/// (SPEC §2): bound to `(head, base, check set)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub change: ChangeId,
    pub head: CommitId,
    pub base: CommitId,
    pub check_set: String,
    pub submitter: String,
}

/// The result of merging a change onto a base commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean(TreeId),
    Conflicted(Vec<String>),
}

/// The single commit a land writes onto the base (SPEC D15).
#[derive(Clone, Debug)]
pub struct LandedCommit {
    pub tree: TreeId,
    pub parent: CommitId,
    pub change: ChangeId,
    pub owner: String,
    pub head: CommitId,
    pub receipt: Option<Receipt>,
    pub approved_head: CommitId,
}

impl LandedCommit {
    /// Commit message with the change, receipt and approval trailers (D15).
    pub fn message(&self) -> String {
        let mut text = format!(
            "Land change {}\n\nRosen-Change: {}\nRosen-Owner: {}\nRosen-Head: {}\n",
            self.change, self.change, self.owner, self.head
        );
        if let Some(receipt) = &self.receipt {
            text.push_str(&format!(
                "Rosen-Receipt: {} {} {}\n",
                receipt.check_set, receipt.base, receipt.submitter
            ));
        }
        text.push_str(&format!("Rosen-Approval: {}\n", self.approved_head));
        text
    }
}

/// Events landing emits (SPEC R9). Routing to subscribers is the event bus's job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingEvent {
    Landed {
        change: ChangeId,
        commit: CommitId,
    },
    Rebased {
        change: ChangeId,
        onto: CommitId,
    },
    Conflict {
        change: ChangeId,
        paths: Vec<String>,
    },
    Released {
        change: ChangeId,
    },
}

/// Errors from landing or one of its collaborators.
#[derive(Debug)]
pub enum LandError {
    UnknownChange(ChangeId),
    NoBase(String),
    InvalidReceipt(String),
    /// The compare-and-swap kept losing races for every allowed attempt.
    Contended {
        attempts: u32,
    },
    Store(String),
    Backend(String),
}

impl fmt::Display for LandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LandError::UnknownChange(id) => write!(f, "unknown change {id}"),
            LandError::NoBase(name) => write!(f, "base ref {name} is not initialised"),
            LandError::InvalidReceipt(why) => write!(f, "invalid receipt: {why}"),
            LandError::Contended { attempts } => {
                write!(f, "base kept moving; gave up after {attempts} attempts")
            }
            LandError::Store(why) => write!(f, "metadata store: {why}"),
            LandError::Backend(why) => write!(f, "backend: {why}"),
        }
    }
}

impl std::error::Error for LandError {}

impl From<rusqlite::Error> for LandError {
    fn from(error: rusqlite::Error) -> Self {
        LandError::Store(error.to_string())
    }
}

pub type LandResult<T> = Result<T, LandError>;

/// The change model, as landing sees it.
pub trait ChangeBook: Send + Sync {
    fn facts(&self, id: &ChangeId) -> LandResult<Option<ChangeFacts>>;
    /// Changes whose parent is `id`, bottom of each stack first.
    fn stacked_on(&self, id: &ChangeId) -> LandResult<Vec<ChangeId>>;
    /// Unlanded changes that overlap `id` (SPEC §2 "Overlap").
    fn overlapping(&self, id: &ChangeId) -> LandResult<Vec<ChangeId>>;
    fn mark_landed(&self, id: &ChangeId, commit: &CommitId) -> LandResult<()>;
    fn record_conflicts(&self, id: &ChangeId, paths: &[String]) -> LandResult<()>;
    /// Moves a change onto `onto` without rewriting its commits (R8 step 4).
    /// `merged` is the clean merge result, or `None` when it conflicted.
    fn reparent(&self, id: &ChangeId, onto: &CommitId, merged: Option<&TreeId>) -> LandResult<()>;
    fn release_claims(&self, id: &ChangeId) -> LandResult<()>;
}

/// Three-way merge (SPEC R5): `head`'s edits since `merge_base`, onto `onto`.
pub trait Merger: Send + Sync {
    fn merge(
        &self,
        merge_base: &CommitId,
        onto: &CommitId,
        head: &CommitId,
    ) -> LandResult<MergeOutcome>;
}

/// Writes the landed commit object to the object store.
pub trait CommitWriter: Send + Sync {
    fn write_landed(&self, commit: &LandedCommit) -> LandResult<CommitId>;
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: LandingEvent);
}
