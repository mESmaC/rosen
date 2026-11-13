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

mod conflict_routes;
mod engine;
mod ledger;
pub(crate) use ledger::ensure_schema;
mod overlap_targets;
pub mod reach;
mod reach_store;
mod restack;
mod review_gate;
pub mod rules;
pub mod service;

use std::fmt;

pub use engine::{LandOutcome, Landing, LandingConfig, LandingDeps, MergePreview, RecoveryReport};
pub use ledger::{CasResult, InitBase, LandRecord, Ledger};
pub use restack::RestackReport;
pub use review_gate::StoredReviews;
pub use rules::{LandInput, ReceiptCheck, Refusal, Verdict};
pub use service::{LandingService, ReviewGate};

pub use rosen_core::{
    AgentId, ChangeId, ChangeParent, ChangeState, LandedCommit, ObjectId, Receipt,
};

/// A commit object ID. An alias of [`ObjectId`] that names the role.
pub type CommitId = ObjectId;
/// A tree object ID. An alias of [`ObjectId`] that names the role.
pub type TreeId = ObjectId;

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

/// The facts landing reads about one change from the change model.
#[derive(Clone, Debug)]
pub struct ChangeFacts {
    pub id: ChangeId,
    pub owner: AgentId,
    /// The change's description, which becomes the landed commit's message.
    pub description: String,
    pub state: ChangeState,
    pub parent: ChangeParent,
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
    /// Who approved the change at `approved_head` (the D15 approval trailers).
    pub approvers: Vec<AgentId>,
}

/// The result of merging a change onto a base commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean(TreeId),
    Conflicted(Vec<String>),
}

/// Whether [`ChangeBook::record_conflicts_at`] attached the conflicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routing {
    Stored,
    /// A snapshot moved the head, or the change closed, after the merge read
    /// it. Nothing was stored.
    ChangeMoved,
}

/// What [`ChangeBook::reparent`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reparent {
    Moved,
    /// The change already sits on `onto` or a later base: a replayed or
    /// slower post-land run must not move it back, so its rebase is dropped.
    AlreadyPast,
    /// A snapshot moved the head after the merge read it; merge again.
    HeadMoved,
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
    /// Claims released by the land that made `commit`.
    Released {
        change: ChangeId,
        commit: CommitId,
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

impl From<rosen_core::Error> for LandError {
    fn from(error: rosen_core::Error) -> Self {
        LandError::Store(error.to_string())
    }
}

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
    /// Records conflicts against the exact immutable inputs of this attempt.
    /// Stores nothing and returns [`Routing::ChangeMoved`] when the change
    /// no longer has `head` or has closed, so the caller retries on its new
    /// state instead of attaching conflicts from an older head.
    fn record_conflicts_at(
        &self,
        id: &ChangeId,
        merge_base: &CommitId,
        onto: &CommitId,
        head: &CommitId,
        paths: &[String],
    ) -> LandResult<Routing> {
        let _ = (merge_base, onto, head);
        self.record_conflicts(id, paths).map(|()| Routing::Stored)
    }

    /// Moves a change onto `onto` without rewriting its commits (R8 step 4).
    /// `head` is the head the merge read; `merged` is the clean merge result,
    /// or `None` when it conflicted, which only asks what a move would do.
    /// Never moves a change backwards or from a head it no longer has.
    fn reparent(
        &self,
        id: &ChangeId,
        onto: &CommitId,
        head: &CommitId,
        merged: Option<&TreeId>,
    ) -> LandResult<Reparent>;
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

    /// Emits an event that happens once per land (`Landed`, `Released`)
    /// unless it was already emitted, so replaying a land's post-land work
    /// after a crash does not announce the land twice.
    fn emit_once(&self, event: LandingEvent) {
        self.emit(event);
    }
}
