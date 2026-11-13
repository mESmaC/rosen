//! The land loop of SPEC R8: gather, merge, decide, write, compare-and-swap,
//! and retry with backoff when another land wins the race.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rosen_core::history::{LandedTrailers, ReceiptTrailer};
use rosen_core::{ReceiptRefusal, Signature, PRE_LAND_CHECK_SET};

use super::ledger::ReceiptLookup;
use super::restack::Restacker;
use super::rules::{self, LandInput, ReceiptCheck, Refusal, Verdict};
use super::{
    CasResult, ChangeBook, ChangeFacts, ChangeId, ChangeParent, CommitId, CommitWriter, EventSink,
    LandError, LandRecord, LandResult, LandedCommit, LandingEvent, Ledger, MergeOutcome, Merger,
    Receipt, RestackReport, Routing, Tier, TreeId,
};

#[derive(Clone, Debug)]
pub struct LandingConfig {
    /// The project's base ref, for example `main`.
    pub base_ref: String,
    /// Name of the pre-land check set receipts must cover (D20).
    pub pre_land_check_set: String,
    pub max_attempts: u32,
    pub backoff_start: Duration,
    pub backoff_cap: Duration,
}

impl Default for LandingConfig {
    fn default() -> Self {
        LandingConfig {
            base_ref: "main".into(),
            pre_land_check_set: PRE_LAND_CHECK_SET.into(),
            // A land must commit in under 200 ms (R8); these bounds keep the
            // worst-case backoff total well inside that.
            max_attempts: 12,
            backoff_start: Duration::from_millis(1),
            backoff_cap: Duration::from_millis(16),
        }
    }
}

/// The server pieces landing calls into.
pub struct LandingDeps {
    pub book: Box<dyn ChangeBook>,
    pub merger: Box<dyn Merger>,
    pub writer: Box<dyn CommitWriter>,
    pub events: Box<dyn EventSink>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LandOutcome {
    Landed {
        commit: CommitId,
        attempts: u32,
        /// Post-land rebase result. A failure here never undoes the land;
        /// [`Landing::recover`] replays it.
        restack: Result<RestackReport, String>,
    },
    AlreadyLanded {
        commit: CommitId,
    },
    Refused(Refusal),
}

/// The server-computed merge result a checker builds (R8 step 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergePreview {
    pub base: CommitId,
    pub outcome: MergeOutcome,
}

enum Attempt {
    Done(LandOutcome),
    /// The base or the change moved after this attempt read it; decide again.
    Retry,
}

/// Decision inputs read for one attempt, with the receipt they rely on.
struct Gathered {
    input: LandInput,
    receipt: Option<Receipt>,
}

/// What one [`Landing::recover`] pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Lands whose post-land work this pass finished.
    pub finished: Vec<ChangeId>,
    /// Lands whose post-land work failed again, with why. They stay pending
    /// for the next pass.
    pub failed: Vec<(ChangeId, String)>,
    /// Lands another thread was already finishing.
    pub busy: Vec<ChangeId>,
}

impl RecoveryReport {
    /// True when no land this pass saw is left unfinished by it.
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty() && self.busy.is_empty()
    }
}

pub struct Landing {
    ledger: Ledger,
    deps: LandingDeps,
    config: LandingConfig,
    recheck_store: Option<std::sync::Arc<dyn rosen_store::ObjectStore>>,
    /// Lands whose post-land work is running now. A recovery pass and the
    /// land's own request, or two passes, never run it at once, so one land
    /// never releases, rebases or announces twice in parallel.
    post_landing: Mutex<HashSet<ChangeId>>,
}

impl Landing {
    pub fn new(ledger: Ledger, deps: LandingDeps, config: LandingConfig) -> Landing {
        Landing {
            ledger,
            deps,
            config,
            recheck_store: None,
            post_landing: Mutex::new(HashSet::new()),
        }
    }

    /// Enables build-unit receipt reuse using immutable objects from the
    /// authoritative store. Without it, only intervening prose is reusable.
    pub fn with_recheck_store(
        mut self,
        store: std::sync::Arc<dyn rosen_store::ObjectStore>,
    ) -> Self {
        self.recheck_store = Some(store);
        self
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn config(&self) -> &LandingConfig {
        &self.config
    }

    /// Stores a receipt after checking it is bound to the change's current
    /// head, the configured check set, and a commit the base has held.
    /// Returns the stored receipt, which keeps the first submitter when the
    /// same receipt was already stored.
    pub fn submit_receipt(&self, receipt: Receipt) -> LandResult<Receipt> {
        let facts = self.facts(&receipt.change)?;
        receipt
            .check_binding(&facts.head, facts.state, &self.config.pre_land_check_set)
            .map_err(|refusal| LandError::InvalidReceipt(refusal.to_string()))?;
        let base_ref = &self.config.base_ref;
        if self.ledger.base_seq(base_ref, &receipt.base)?.is_none() {
            let refusal = ReceiptRefusal::UnknownBase {
                base: receipt.base,
                base_ref: base_ref.clone(),
            };
            return Err(LandError::InvalidReceipt(refusal.to_string()));
        }
        self.ledger.put_receipt(&receipt)
    }

    pub fn merge_preview(&self, change: &ChangeId) -> LandResult<MergePreview> {
        let facts = self.facts(change)?;
        let base = self.current_base()?;
        let outcome = self
            .deps
            .merger
            .merge(&facts.merge_base, &base, &facts.head)?;
        Ok(MergePreview { base, outcome })
    }

    /// Lands a change (R8 step 2). Retries a lost compare-and-swap with
    /// jittered exponential backoff; each retry re-decides, so a retry stops
    /// as soon as the moved base needs a new receipt.
    pub fn land(&self, change: &ChangeId) -> LandResult<LandOutcome> {
        let mut delay = self.config.backoff_start;
        for attempt in 1..=self.config.max_attempts {
            if let Attempt::Done(outcome) = self.attempt(change, attempt)? {
                return Ok(outcome);
            }
            thread::sleep(jitter(delay, change, attempt));
            delay = (delay * 2).min(self.config.backoff_cap);
        }
        Err(LandError::Contended {
            attempts: self.config.max_attempts,
        })
    }

    /// Replays post-land work that a crash or a failed step interrupted,
    /// oldest land first. One land's failure does not hold back the others:
    /// it stays pending and the next pass retries it. Replays are safe in
    /// any order because every step is idempotent and a rebase only ever
    /// moves a change forward ([`ChangeBook::reparent`]).
    pub fn recover(&self) -> LandResult<RecoveryReport> {
        let mut report = RecoveryReport::default();
        for record in self.ledger.pending_post_land(&self.config.base_ref)? {
            match self.post_land(&record, true) {
                Ok(Some(_)) => report.finished.push(record.change),
                Ok(None) => report.busy.push(record.change),
                Err(error) => report.failed.push((record.change, error.to_string())),
            }
        }
        Ok(report)
    }

    fn attempt(&self, change: &ChangeId, attempt: u32) -> LandResult<Attempt> {
        if let Some(commit) = self.ledger.landing_of(change)? {
            return Ok(Attempt::Done(LandOutcome::AlreadyLanded { commit }));
        }
        let facts = self.facts(change)?;
        let base = self.current_base()?;
        // A receipt on a base newer than the one read means the base moved
        // mid-attempt; retrying reads that base instead of failing the land.
        let Some(mut gathered) = self.gather(&facts, &base)? else {
            return Ok(Attempt::Retry);
        };
        if !rules::needs_merge(&gathered.input) {
            return Ok(Attempt::Done(refused(rules::decide(&gathered.input))));
        }
        let outcome = self
            .deps
            .merger
            .merge(&facts.merge_base, &base, &facts.head)?;
        gathered.input.merge_clean = matches!(outcome, MergeOutcome::Clean(_));
        match (rules::decide(&gathered.input), outcome) {
            (Verdict::Land, MergeOutcome::Clean(tree)) => {
                self.commit_land(&facts, base, tree, gathered.receipt, attempt)
            }
            (Verdict::Refuse(Refusal::MergeConflict), MergeOutcome::Conflicted(paths)) => {
                let routing = self.deps.book.record_conflicts_at(
                    change,
                    &facts.merge_base,
                    &base,
                    &facts.head,
                    &paths,
                )?;
                if routing == Routing::ChangeMoved {
                    // A snapshot raced the merge: its conflicts belong to an
                    // older head. The retry reads the new head and state.
                    return Ok(Attempt::Retry);
                }
                self.deps.events.emit(LandingEvent::Conflict {
                    change: *change,
                    paths,
                });
                Ok(Attempt::Done(LandOutcome::Refused(Refusal::MergeConflict)))
            }
            (verdict, _) => Ok(Attempt::Done(refused(verdict))),
        }
    }

    /// Reads the decision inputs, or `None` when the change's newest receipt
    /// is bound to a base the ref reached after `base` was read.
    fn gather(&self, facts: &ChangeFacts, base: &CommitId) -> LandResult<Option<Gathered>> {
        let parent_landed = match &facts.parent {
            ChangeParent::Base(_) => true,
            ChangeParent::Change(parent) => self.ledger.landing_of(parent)?.is_some(),
        };
        let mut input = LandInput {
            state: facts.state,
            parent_landed,
            has_conflicts: facts.has_conflicts,
            approved: facts.approved_head == Some(facts.head),
            tier: facts.tier,
            receipt: ReceiptCheck::Missing,
            base_moved: false,
            landed_since_all_independent: true,
            // Filled in once the merge has run.
            merge_clean: true,
        };
        if facts.tier == Tier::ProseOnly {
            return Ok(Some(Gathered {
                input,
                receipt: None,
            }));
        }
        let lookup = self.ledger.best_receipt(
            &self.config.base_ref,
            &facts.id,
            &facts.head,
            &self.config.pre_land_check_set,
            base,
        )?;
        let receipt = match lookup {
            ReceiptLookup::Ahead => return Ok(None),
            ReceiptLookup::Missing => None,
            ReceiptLookup::Invalid => {
                input.receipt = ReceiptCheck::Invalid;
                None
            }
            ReceiptLookup::Valid { receipt, base_seq } => {
                input.receipt = ReceiptCheck::Valid;
                input.base_moved = receipt.base != *base;
                input.landed_since_all_independent =
                    self.independent_since(facts, base, base_seq)?;
                Some(receipt)
            }
        };
        Ok(Some(Gathered { input, receipt }))
    }

    fn independent_since(
        &self,
        facts: &ChangeFacts,
        base: &CommitId,
        seq: i64,
    ) -> LandResult<bool> {
        let lands = self
            .ledger
            .lands_between(&self.config.base_ref, seq, base)?;
        if lands.iter().all(|(_, tier)| *tier == Tier::ProseOnly) {
            return Ok(true);
        }
        match &self.recheck_store {
            Some(store) => super::reach_store::reusable(store.as_ref(), base, facts, &lands),
            None => Ok(false),
        }
    }

    fn commit_land(
        &self,
        facts: &ChangeFacts,
        base: CommitId,
        tree: TreeId,
        receipt: Option<Receipt>,
        attempts: u32,
    ) -> LandResult<Attempt> {
        let landed = landed_commit(facts, base, tree, receipt.as_ref())?;
        // Written before the swap: if the swap loses, the object is an
        // unreferenced orphan, which is harmless because objects are immutable.
        let commit = self.deps.writer.write_landed(&landed)?;
        let record = LandRecord {
            ref_name: self.config.base_ref.clone(),
            change: facts.id,
            head: facts.head,
            commit,
            tier: facts.tier,
        };
        match self.ledger.compare_and_swap(&base, &record)? {
            CasResult::Swapped { .. } => {
                // Nobody else can know of this land yet, so the run is ours;
                // a concurrent recovery pass that saw it first finishes it.
                let restack = self
                    .post_land(&record, false)
                    .map(Option::unwrap_or_default)
                    .map_err(|error| error.to_string());
                Ok(Attempt::Done(LandOutcome::Landed {
                    commit,
                    attempts,
                    restack,
                }))
            }
            // A moved change is decided afresh, like a moved base: the retry
            // reads its new head and state, and refuses if the receipt or
            // approval no longer covers them.
            CasResult::BaseMoved { .. } | CasResult::ChangeMoved => Ok(Attempt::Retry),
            CasResult::OpenConflicts => Ok(Attempt::Done(LandOutcome::Refused(Refusal::Conflicts))),
            CasResult::AlreadyLanded { commit } => {
                Ok(Attempt::Done(LandOutcome::AlreadyLanded { commit }))
            }
        }
    }

    /// Runs a land's post-land work (R8 step 4), or returns `None` when
    /// another thread is running it now. A `replay` may follow a run that
    /// announced the land before it crashed or failed, so it announces only
    /// what the event log does not hold yet; a land's first run cannot have
    /// announced anything, and skips that lookup.
    fn post_land(&self, record: &LandRecord, replay: bool) -> LandResult<Option<RestackReport>> {
        let Some(_running) = PostLandRun::start(&self.post_landing, record.change) else {
            return Ok(None);
        };
        // Read again under the run: a run that finished between the pending
        // list being read and this one starting has nothing left to do.
        if !self.ledger.post_land_pending(&record.change)? {
            return Ok(Some(RestackReport::default()));
        }
        let landed = LandingEvent::Landed {
            change: record.change,
            commit: record.commit,
        };
        if replay {
            self.deps.events.emit_once(landed);
        } else {
            self.deps.events.emit(landed);
        }
        let restacker = Restacker {
            book: self.deps.book.as_ref(),
            merger: self.deps.merger.as_ref(),
            events: self.deps.events.as_ref(),
            replay,
        };
        let report = restacker.after_land(&record.change, &record.commit)?;
        self.ledger.mark_post_land_done(&record.change)?;
        Ok(Some(report))
    }

    fn facts(&self, change: &ChangeId) -> LandResult<ChangeFacts> {
        self.deps
            .book
            .facts(change)?
            .ok_or(LandError::UnknownChange(*change))
    }

    fn current_base(&self) -> LandResult<CommitId> {
        self.ledger
            .base(&self.config.base_ref)?
            .ok_or_else(|| LandError::NoBase(self.config.base_ref.clone()))
    }
}

/// Marks one land's post-land work as running until dropped, including when
/// the work fails or panics.
struct PostLandRun<'a> {
    running: &'a Mutex<HashSet<ChangeId>>,
    change: ChangeId,
}

impl<'a> PostLandRun<'a> {
    fn start(running: &'a Mutex<HashSet<ChangeId>>, change: ChangeId) -> Option<Self> {
        let mut set = running.lock().unwrap_or_else(PoisonError::into_inner);
        set.insert(change)
            .then_some(PostLandRun { running, change })
    }
}

impl Drop for PostLandRun<'_> {
    fn drop(&mut self) {
        let mut set = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        set.remove(&self.change);
    }
}

/// The single landed commit (D15): the merged tree on top of `base`, authored
/// by the change's owner and committed by the server.
fn landed_commit(
    facts: &ChangeFacts,
    base: CommitId,
    tree: TreeId,
    receipt: Option<&Receipt>,
) -> LandResult<LandedCommit> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let owner = facts.owner.as_str();
    // Submission already refuses a malformed check set; this keeps a stored
    // row that slipped past it from forging trailers (issue #18).
    let receipt = receipt
        .map(|r| ReceiptTrailer::new(&r.check_set, r.head, r.base, r.submitter.clone()))
        .transpose()
        .map_err(|err| LandError::InvalidReceipt(err.to_string()))?;
    Ok(LandedCommit {
        base,
        tree,
        author: Signature::new(owner, format!("{owner}@rosen"), now),
        committer: Signature::new("rosen-server", "rosen-server@rosen", now),
        description: facts.description.clone(),
        trailers: LandedTrailers {
            change: facts.id,
            receipt,
            approvers: facts.approvers.clone(),
        },
    })
}

fn refused(verdict: Verdict) -> LandOutcome {
    match verdict {
        Verdict::Refuse(refusal) => LandOutcome::Refused(refusal),
        // Unreachable in practice: `Land` always comes with a clean merge.
        Verdict::Land => LandOutcome::Refused(Refusal::MergeConflict),
    }
}

/// Scales `delay` into [delay/2, delay] by a hash of the change and attempt,
/// so changes that lost the same race do not retry in lockstep.
fn jitter(delay: Duration, change: &ChangeId, attempt: u32) -> Duration {
    let mut hasher = DefaultHasher::new();
    change.hash(&mut hasher);
    attempt.hash(&mut hasher);
    let fraction = (hasher.finish() % 1024) as u32;
    delay / 2 + delay * fraction / 2048
}
