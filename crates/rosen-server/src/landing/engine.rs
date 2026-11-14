//! The land loop of SPEC R8: gather, merge, decide, write, compare-and-swap,
//! and retry with backoff when another land wins the race.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rosen_core::history::{LandedTrailers, ReceiptTrailer};
use rosen_core::{ReceiptRefusal, Signature, PRE_LAND_CHECK_SET};

use super::receipt_store::{ReceiptLookup, SignedReceipt};
use super::restack::Restacker;
use super::rules::{self, LandInput, ReceiptCheck, Refusal, Verdict};
use super::{
    CasResult, ChangeBook, ChangeFacts, ChangeId, ChangeParent, CommitId, CommitWriter, EventSink,
    LandError, LandRecord, LandResult, LandedCommit, LandingEvent, Ledger, MergeOutcome, Merger,
    RestackReport, Routing, Tier, TreeId,
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

/// A land whose base move has committed but whose post-land work (R8 step
/// 4) has not run yet. The ledger marks it pending in the same transaction,
/// so a crash before [`Landing::finish_land`] leaves it to recovery.
#[derive(Debug)]
pub struct CommittedLand {
    pub record: LandRecord,
    pub attempts: u32,
}

/// How a land request ended before any post-land work.
#[derive(Debug)]
pub enum Decision {
    /// This request moved the base; its post-land work is still to run.
    Committed(CommittedLand),
    /// Already landed, or refused: nothing is left to run.
    Decided(LandOutcome),
}

enum Attempt {
    Done(LandOutcome),
    Committed(CommittedLand),
    /// The base or the change moved after this attempt read it; decide again.
    Retry,
}

/// Decision inputs read for one attempt, with the receipt they rely on.
struct Gathered {
    input: LandInput,
    receipt: Option<SignedReceipt>,
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
    /// never releases, rebases or announces twice in parallel. Shared with
    /// the engine that replaces this one ([`Landing::taking_over`]).
    post_landing: Arc<Mutex<HashSet<CommitId>>>,
    /// Build units of the last base re-checked, carried to the engine that
    /// replaces this one so a config reload does not cost a full tree walk.
    units_cache: Arc<super::reach_store::UnitsCache>,
}

impl Landing {
    pub fn new(ledger: Ledger, deps: LandingDeps, config: LandingConfig) -> Landing {
        Landing {
            ledger,
            deps,
            config,
            recheck_store: None,
            post_landing: Arc::new(Mutex::new(HashSet::new())),
            units_cache: Arc::default(),
        }
    }

    /// Shares `old`'s record of running post-land work, so a run still going
    /// on the engine this one replaces keeps excluding runs of the same land.
    pub fn taking_over(mut self, old: &Landing) -> Self {
        self.post_landing = Arc::clone(&old.post_landing);
        self.units_cache = Arc::clone(&old.units_cache);
        self
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
    /// head, the configured check set, and a commit the base has held, and
    /// that its machine's key verifies its signature. Returns the stored
    /// receipt and its machine, which keep the first submitter when the same
    /// receipt was already stored.
    pub fn submit_receipt(&self, signed: SignedReceipt) -> LandResult<SignedReceipt> {
        let receipt = &signed.receipt;
        let facts = self.facts(&receipt.change)?;
        let required = self.check_set_at(&self.current_base()?)?;
        receipt
            .check_binding(&facts.head, facts.state, &required)
            .map_err(|refusal| LandError::InvalidReceipt(refusal.to_string()))?;
        let base_ref = &self.config.base_ref;
        if self.ledger.base_seq(base_ref, &receipt.base)?.is_none() {
            let refusal = ReceiptRefusal::UnknownBase {
                base: receipt.base,
                base_ref: base_ref.clone(),
            };
            return Err(LandError::InvalidReceipt(refusal.to_string()));
        }
        self.ledger.put_receipt(&signed)
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
    /// as soon as the moved base needs a new receipt. Runs the land's
    /// post-land work before returning.
    pub fn land(&self, change: &ChangeId) -> LandResult<LandOutcome> {
        Ok(match self.decide(change)? {
            Decision::Committed(land) => {
                let commit = land.record.commit;
                let attempts = land.attempts;
                LandOutcome::Landed {
                    commit,
                    attempts,
                    restack: self
                        .post_land(&land.record, false)
                        .map(Option::unwrap_or_default)
                        .map_err(|error| error.to_string()),
                }
            }
            Decision::Decided(outcome) => outcome,
        })
    }

    /// Lands a change like [`Landing::land`], but returns once the base move
    /// commits, so the requester hears of the land without waiting for the
    /// rebases of other changes. The caller runs [`Landing::restack_stacked`]
    /// and then [`Landing::finish_land`]; recovery replays the work if the
    /// caller never does.
    pub fn decide(&self, change: &ChangeId) -> LandResult<Decision> {
        let mut delay = self.config.backoff_start;
        for attempt in 1..=self.config.max_attempts {
            match self.attempt(change, attempt)? {
                Attempt::Done(outcome) => return Ok(Decision::Decided(outcome)),
                Attempt::Committed(land) => return Ok(Decision::Committed(land)),
                Attempt::Retry => {}
            }
            thread::sleep(jitter(delay, change, attempt));
            delay = (delay * 2).min(self.config.backoff_cap);
        }
        Err(LandError::Contended {
            attempts: self.config.max_attempts,
        })
    }

    /// The part of a committed land's post-land work its requester waits
    /// for: announces the land and rebases the changes stacked on it. A
    /// stacked child's merge result reads its parent link, recorded parent
    /// tree and head separately, so the child must not move under a checker
    /// that asks for it right after hearing that its parent landed.
    /// Does nothing when a recovery pass is running the land's work now, or
    /// has already finished it.
    pub fn restack_stacked(&self, land: &CommittedLand) -> Result<RestackReport, String> {
        let record = &land.record;
        let Some(_running) = PostLandRun::start(&self.post_landing, record.commit) else {
            return Ok(RestackReport::default());
        };
        // A finished run announced the land; announcing it here would repeat it.
        match self
            .ledger
            .post_land_pending(&record.ref_name, &record.commit)
        {
            Ok(true) => {}
            Ok(false) => return Ok(RestackReport::default()),
            Err(error) => return Err(error.to_string()),
        }
        self.deps.events.emit(LandingEvent::Landed {
            change: record.change,
            commit: record.commit,
        });
        self.restacker(false)
            .rebase_stacked(&record.change, &record.commit)
            .map_err(|error| error.to_string())
    }

    /// Runs the rest of a land's post-land work after
    /// [`Landing::restack_stacked`]. It replays that run, so it announces
    /// the land only if the event log does not hold it yet. A failure never
    /// undoes the land; [`Landing::recover`] replays it.
    pub fn finish_land(&self, land: &CommittedLand) -> Result<RestackReport, String> {
        self.post_land(&land.record, true)
            .map(Option::unwrap_or_default)
            .map_err(|error| error.to_string())
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
        let mut facts = self.facts(change)?;
        let base = self.current_base()?;
        facts.tier = self.deps.book.tier_at(&facts, &base)?;
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
            &self.check_set_at(base)?,
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
                input.base_moved = receipt.receipt.base != *base;
                input.landed_since_all_independent =
                    self.independent_since(facts, base, base_seq)?;
                Some(receipt)
            }
        };
        Ok(Some(Gathered { input, receipt }))
    }

    /// Resolve against the attempt's immutable base. A cached engine can
    /// outlive a config land, including when its own CAS loses that race.
    pub(crate) fn check_set_at(&self, base: &CommitId) -> LandResult<String> {
        match &self.recheck_store {
            Some(store) => crate::project_config::resolve(store.as_ref(), base)
                .map(|resolved| resolved.config.checks.pre_land.name.clone())
                .map_err(|error| LandError::Backend(error.to_string())),
            None => Ok(self.config.pre_land_check_set.clone()),
        }
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
            Some(store) => {
                super::reach_store::reusable(store.as_ref(), &self.units_cache, base, facts, &lands)
            }
            None => Ok(false),
        }
    }

    fn commit_land(
        &self,
        facts: &ChangeFacts,
        base: CommitId,
        tree: TreeId,
        receipt: Option<SignedReceipt>,
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
        let swap = self
            .ledger
            .compare_and_swap_with(&base, &record, receipt.as_ref())?;
        match swap {
            CasResult::Swapped { .. } => Ok(Attempt::Committed(CommittedLand { record, attempts })),
            // A moved change is decided afresh, like a moved base: the retry
            // reads its new head and state, and refuses if the receipt or
            // approval no longer covers them.
            // An untrusted receipt is decided afresh too: the retry finds it
            // invalid and refuses the land.
            CasResult::BaseMoved { .. } | CasResult::ChangeMoved | CasResult::ReceiptUntrusted => {
                Ok(Attempt::Retry)
            }
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
        let Some(_running) = PostLandRun::start(&self.post_landing, record.commit) else {
            return Ok(None);
        };
        // Read again under the run: a run that finished between the pending
        // list being read and this one starting has nothing left to do.
        if !self
            .ledger
            .post_land_pending(&record.ref_name, &record.commit)?
        {
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
        let report = self
            .restacker(replay)
            .after_land(&record.change, &record.commit)?;
        self.ledger
            .mark_post_land_done(&record.ref_name, &record.commit)?;
        Ok(Some(report))
    }

    fn restacker(&self, replay: bool) -> Restacker<'_> {
        Restacker {
            book: self.deps.book.as_ref(),
            merger: self.deps.merger.as_ref(),
            events: self.deps.events.as_ref(),
            replay,
        }
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
/// the work fails or panics. Keyed by the landed commit, so a change that
/// lands again after a revert (R12) runs its second land's work separately.
struct PostLandRun<'a> {
    running: &'a Mutex<HashSet<CommitId>>,
    commit: CommitId,
}

impl<'a> PostLandRun<'a> {
    fn start(running: &'a Mutex<HashSet<CommitId>>, commit: CommitId) -> Option<Self> {
        let mut set = running.lock().unwrap_or_else(PoisonError::into_inner);
        // Lazily: a guard built for a run already in the set would drop
        // here, relocking `running` while it is held and removing the other
        // run's entry.
        set.insert(commit).then(|| PostLandRun { running, commit })
    }
}

impl Drop for PostLandRun<'_> {
    fn drop(&mut self) {
        let mut set = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        set.remove(&self.commit);
    }
}

/// The single landed commit (D15): the merged tree on top of `base`, authored
/// by the change's owner and committed by the server.
fn landed_commit(
    facts: &ChangeFacts,
    base: CommitId,
    tree: TreeId,
    receipt: Option<&SignedReceipt>,
) -> LandResult<LandedCommit> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let owner = facts.owner.as_str();
    // Submission already refuses a malformed check set; this keeps a stored
    // row that slipped past it from forging trailers (issue #18).
    let receipt = receipt
        .map(receipt_trailer)
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

/// The `Rosen-Receipt` trailer, naming the submitter as nick plus the
/// machine whose credential and key the receipt came under (R14).
fn receipt_trailer(signed: &SignedReceipt) -> rosen_core::Result<ReceiptTrailer> {
    let r = &signed.receipt;
    let submitter = r.submitter.on_machine(&signed.machine)?;
    ReceiptTrailer::new(&r.check_set, r.head, r.base, submitter)
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
