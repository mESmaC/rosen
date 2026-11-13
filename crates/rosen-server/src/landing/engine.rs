//! The land loop of SPEC R8: gather, merge, decide, write, compare-and-swap,
//! and retry with backoff when another land wins the race.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rosen_core::history::LandedTrailers;
use rosen_core::{ReceiptRefusal, Signature, PRE_LAND_CHECK_SET};

use super::ledger::ReceiptLookup;
use super::restack::Restacker;
use super::rules::{self, LandInput, ReceiptCheck, Refusal, Verdict};
use super::{
    CasResult, ChangeBook, ChangeFacts, ChangeId, ChangeParent, CommitId, CommitWriter, EventSink,
    LandError, LandRecord, LandResult, LandedCommit, LandingEvent, Ledger, MergeOutcome, Merger,
    Receipt, RestackReport, Tier, TreeId,
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
    BaseMoved,
}

/// Decision inputs read for one attempt, with the receipt they rely on.
struct Gathered {
    input: LandInput,
    receipt: Option<Receipt>,
}

pub struct Landing {
    ledger: Ledger,
    deps: LandingDeps,
    config: LandingConfig,
}

impl Landing {
    pub fn new(ledger: Ledger, deps: LandingDeps, config: LandingConfig) -> Landing {
        Landing {
            ledger,
            deps,
            config,
        }
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

    /// Replays post-land work that a crash interrupted. Returns how many
    /// lands it finished.
    pub fn recover(&self) -> LandResult<usize> {
        let pending = self.ledger.pending_post_land(&self.config.base_ref)?;
        for record in &pending {
            self.post_land(record)?;
        }
        Ok(pending.len())
    }

    fn attempt(&self, change: &ChangeId, attempt: u32) -> LandResult<Attempt> {
        if let Some(commit) = self.ledger.landing_of(change)? {
            return Ok(Attempt::Done(LandOutcome::AlreadyLanded { commit }));
        }
        let facts = self.facts(change)?;
        let base = self.current_base()?;
        let mut gathered = self.gather(&facts, &base)?;
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
                self.deps.book.record_conflicts(change, &paths)?;
                self.deps.events.emit(LandingEvent::Conflict {
                    change: *change,
                    paths,
                });
                Ok(Attempt::Done(LandOutcome::Refused(Refusal::MergeConflict)))
            }
            (verdict, _) => Ok(Attempt::Done(refused(verdict))),
        }
    }

    fn gather(&self, facts: &ChangeFacts, base: &CommitId) -> LandResult<Gathered> {
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
            landed_since_all_prose: true,
            // Filled in once the merge has run.
            merge_clean: true,
        };
        if facts.tier == Tier::ProseOnly {
            return Ok(Gathered {
                input,
                receipt: None,
            });
        }
        let lookup = self.ledger.best_receipt(
            &self.config.base_ref,
            &facts.id,
            &facts.head,
            &self.config.pre_land_check_set,
        )?;
        let receipt = match lookup {
            ReceiptLookup::Missing => None,
            ReceiptLookup::Invalid => {
                input.receipt = ReceiptCheck::Invalid;
                None
            }
            ReceiptLookup::Valid { receipt, base_seq } => {
                input.receipt = ReceiptCheck::Valid;
                input.base_moved = receipt.base != *base;
                input.landed_since_all_prose = self
                    .ledger
                    .landed_since_all_prose(&self.config.base_ref, base_seq)?;
                Some(receipt)
            }
        };
        Ok(Gathered { input, receipt })
    }

    fn commit_land(
        &self,
        facts: &ChangeFacts,
        base: CommitId,
        tree: TreeId,
        receipt: Option<Receipt>,
        attempts: u32,
    ) -> LandResult<Attempt> {
        let landed = landed_commit(facts, base, tree, receipt.as_ref());
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
                let restack = self.post_land(&record).map_err(|error| error.to_string());
                Ok(Attempt::Done(LandOutcome::Landed {
                    commit,
                    attempts,
                    restack,
                }))
            }
            CasResult::BaseMoved { .. } => Ok(Attempt::BaseMoved),
            CasResult::OpenConflicts => Ok(Attempt::Done(LandOutcome::Refused(Refusal::Conflicts))),
            CasResult::AlreadyLanded { commit } => {
                Ok(Attempt::Done(LandOutcome::AlreadyLanded { commit }))
            }
        }
    }

    fn post_land(&self, record: &LandRecord) -> LandResult<RestackReport> {
        self.deps.events.emit(LandingEvent::Landed {
            change: record.change,
            commit: record.commit,
        });
        let restacker = Restacker {
            book: self.deps.book.as_ref(),
            merger: self.deps.merger.as_ref(),
            events: self.deps.events.as_ref(),
        };
        let report = restacker.after_land(&record.change, &record.commit)?;
        self.ledger.mark_post_land_done(&record.change)?;
        Ok(report)
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

/// The single landed commit (D15): the merged tree on top of `base`, authored
/// by the change's owner and committed by the server.
fn landed_commit(
    facts: &ChangeFacts,
    base: CommitId,
    tree: TreeId,
    receipt: Option<&Receipt>,
) -> LandedCommit {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let owner = facts.owner.as_str();
    LandedCommit {
        base,
        tree,
        author: Signature::new(owner, format!("{owner}@rosen"), now),
        committer: Signature::new("rosen-server", "rosen-server@rosen", now),
        description: facts.description.clone(),
        trailers: LandedTrailers {
            change: facts.id,
            receipt: receipt.map(|r| format!("{} {} {}", r.check_set, r.base, r.submitter)),
            approvers: facts.approvers.clone(),
        },
    }
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
