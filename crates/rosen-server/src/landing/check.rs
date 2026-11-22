//! The land check of R8: every gate a land would apply on the current base,
//! read with the land's own functions and decided by [`rules::decide`], but
//! with none of its writes. It never marks a change ready, routes a merge
//! conflict, publishes an event, stores a receipt or moves the base.

use std::collections::BTreeMap;

use rosen_core::ChangeState;

use super::super::reach_store::{self, UnitsReached};
use super::stack::{layer_refusal, stack_top};
use super::*;

/// How many times a check reads again when the base moves under it, the way
/// a land attempt retries, before it reports contention.
const CHECK_READS: u32 = 8;

/// Every gate's verdict for one change on one base.
#[derive(Debug)]
pub struct GateCheck {
    pub base: CommitId,
    /// The change, with the tier its land would record.
    pub top: ChangeFacts,
    pub review: ReviewMode,
    pub recheck: RecheckMode,
    /// The commit the change already landed as.
    pub landed: Option<CommitId>,
    /// Unlanded ancestors bottom first, each with what stops it landing.
    pub ancestors: Vec<(ChangeFacts, Option<Refusal>)>,
    pub reach: UnitsReached,
    pub merge: MergeCheck,
    pub approval: ApprovalCheck,
    pub receipt: ReceiptState,
    /// The first refusal in the land's own order, if the land would refuse.
    pub refusal: Option<CheckRefusal>,
}

impl GateCheck {
    /// True when earlier gates passed, the merge gate stopped at an uncached
    /// complete registered regeneration plan in `after-land` mode, and the
    /// read-only check has not run those commands yet (#454, #468).
    pub fn pending_regeneration(&self) -> bool {
        self.refusal.is_none()
            && self.review == ReviewMode::AfterLand
            && matches!(
                self.merge,
                MergeCheck::Regenerate {
                    auto_at_land: true,
                    ..
                }
            )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MergeCheck {
    /// The merge result of the whole stack.
    Clean(TreeId),
    StoredConflicts,
    Conflicted {
        change: ChangeId,
        paths: Vec<String>,
    },
    Regenerate {
        change: ChangeId,
        paths: Vec<String>,
        merged_tree: TreeId,
        commands: BTreeMap<String, String>,
        /// True when every reported path at this layer has a registered
        /// command, so an after-land request will run the commands
        /// automatically instead of refusing with `regeneration_required`.
        auto_at_land: bool,
    },
    Oversize {
        limit: u64,
        files: Vec<(String, u64)>,
    },
    NotChecked,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApprovalCheck {
    Held,
    Needed,
    NotRequired,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReceiptState {
    NotRequired,
    Missing,
    /// Receipts exist, but none counts for the current head and check set.
    Unbound,
    Valid(SignedReceipt),
    /// The receipt counts across these lands since its base.
    Reusable(SignedReceipt, Vec<CommitId>),
    /// These lands since its base intersect the change's reach.
    Stale(SignedReceipt, Vec<CommitId>),
    NotChecked,
}

/// A refusal the land would answer with: a rule's, or one the land reports
/// from its merge or size check.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckRefusal {
    Rule(Refusal),
    Regenerate {
        change: ChangeId,
        paths: Vec<String>,
        merged_tree: TreeId,
        commands: BTreeMap<String, String>,
    },
    Oversize {
        limit: u64,
        files: Vec<(String, u64)>,
    },
}

impl Landing {
    /// Reports every gate a land of `change` would apply on the current base
    /// (R8 "Land check"). It reads the inputs a land attempt reads, through
    /// the same functions and the read connections, and changes nothing.
    pub fn check(&self, change: &ChangeId) -> LandResult<GateCheck> {
        for _ in 0..CHECK_READS {
            if let Some(check) = self.check_once(change)? {
                return Ok(check);
            }
        }
        Err(LandError::Contended {
            attempts: CHECK_READS,
        })
    }

    /// One read of every gate, or `None` when the base moved under it.
    fn check_once(&self, change: &ChangeId) -> LandResult<Option<GateCheck>> {
        let base = self.current_base()?;
        let policy = self.policy_at(&base)?;
        if let Some(commit) = self.ledger.landing_of(change)? {
            return self.landed_check(change, base, &policy, commit).map(Some);
        }
        let mut stack = self.stack_facts(change)?;
        let mut layers = Vec::new();
        for facts in stack.iter_mut() {
            facts.tier = self.deps.book.tier_at(facts, &base)?;
            layers.push(layer_refusal(facts, policy.review));
        }
        // The receipt and reach are decided on the stack's top as the land
        // decides them; the tier reported is the one the land records.
        let decided = stack_top(&stack);
        let merge = self.merge_check(&stack, &decided, &base)?;
        let gathered = match &merge {
            MergeCheck::Clean(tree) => match self.gather_stack(&stack, *tree, &base, &policy)? {
                Some(gathered) => Some(gathered),
                None => return Ok(None),
            },
            _ => None,
        };
        if self.current_base()? != base {
            return Ok(None);
        }
        let receipt = self.receipt_state(&decided, &base, &merge, gathered.as_ref())?;
        let refusal = first_refusal(
            &layers,
            &decided.id,
            policy.review,
            &merge,
            gathered.as_ref(),
        );
        let reach = self.reach_of(&stack, &decided, &base, &merge)?;
        let approval = approval_check(&decided, policy.review);
        let mut ancestors: Vec<_> = stack.into_iter().zip(layers).collect();
        let (top, _) = ancestors.pop().expect("nonempty stack");
        Ok(Some(GateCheck {
            base,
            review: policy.review,
            recheck: policy.recheck,
            landed: None,
            ancestors,
            reach,
            merge,
            approval,
            receipt,
            refusal,
            top,
        }))
    }

    /// A change that already landed: a land answers `already_landed` and
    /// reads nothing else.
    fn landed_check(
        &self,
        change: &ChangeId,
        base: CommitId,
        policy: &LandPolicy,
        commit: CommitId,
    ) -> LandResult<GateCheck> {
        let mut top = self.facts(change)?;
        top.tier = self.deps.book.tier_at(&top, &base)?;
        Ok(GateCheck {
            base,
            review: policy.review,
            recheck: policy.recheck,
            landed: Some(commit),
            ancestors: Vec::new(),
            reach: UnitsReached::Units(Vec::new()),
            merge: MergeCheck::NotChecked,
            approval: approval_check(&top, policy.review),
            receipt: ReceiptState::NotChecked,
            refusal: Some(CheckRefusal::Rule(Refusal::AlreadyLanded)),
            top,
        })
    }

    /// The stack's merge onto `base`, as the land merges it, then the size
    /// limit on every layer's tree. Conflicts are reported, never routed.
    fn merge_check(
        &self,
        stack: &[ChangeFacts],
        top: &ChangeFacts,
        base: &CommitId,
    ) -> LandResult<MergeCheck> {
        if matches!(top.state, ChangeState::Landed | ChangeState::Abandoned) {
            return Ok(MergeCheck::NotChecked);
        }
        if top.has_conflicts {
            return Ok(MergeCheck::StoredConflicts);
        }
        let mut trees = Vec::new();
        let mut onto = *base;
        for facts in stack {
            match self
                .deps
                .merger
                .merge_layer(&facts.merge_base, &onto, &facts.head, base)?
            {
                MergeOutcome::Clean(tree) => {
                    trees.push(tree);
                    onto = tree;
                }
                MergeOutcome::Conflicted(paths) => {
                    return Ok(MergeCheck::Conflicted {
                        change: facts.id,
                        paths,
                    });
                }
                MergeOutcome::Regenerate { tree, paths } => {
                    let info = self.deps.merger.regeneration_info(base, tree, &paths)?;
                    if let Some(regenerated) = info.cached {
                        trees.push(regenerated);
                        onto = regenerated;
                        continue;
                    }
                    return Ok(MergeCheck::Regenerate {
                        change: facts.id,
                        paths,
                        merged_tree: tree,
                        commands: info.commands,
                        auto_at_land: info.all_registered,
                    });
                }
            }
        }
        let check = match self.check_file_sizes(base, &trees) {
            Ok(()) => MergeCheck::Clean(*trees.last().expect("one tree per layer")),
            Err(LandError::OversizeFiles { limit, files }) => MergeCheck::Oversize { limit, files },
            Err(error) => return Err(error),
        };
        Ok(check)
    }

    /// The receipt gate alone, decided by the land's rule with every other
    /// gate passing, so it names exactly what the receipt contributes.
    fn receipt_state(
        &self,
        top: &ChangeFacts,
        base: &CommitId,
        merge: &MergeCheck,
        gathered: Option<&Gathered>,
    ) -> LandResult<ReceiptState> {
        let (Some(gathered), MergeCheck::Clean(tree)) = (gathered, merge) else {
            return Ok(ReceiptState::NotChecked);
        };
        let receipt_only = LandInput {
            state: ChangeState::Ready,
            parent_landed: true,
            has_conflicts: false,
            approved: true,
            merge_clean: true,
            ..gathered.input
        };
        let signed = gathered.receipt.clone();
        Ok(match (rules::decide(&receipt_only), signed) {
            (Verdict::Refuse(Refusal::NoReceipt), _) => ReceiptState::Missing,
            (Verdict::Refuse(Refusal::NeedsRecheck), Some(signed)) => {
                let since = self.lands_since(&signed, base)?;
                let hit = self.intersecting(top, base, tree, &since)?;
                ReceiptState::Stale(signed, hit)
            }
            (Verdict::Land, None) => ReceiptState::NotRequired,
            (Verdict::Land, Some(signed)) if gathered.input.base_moved => {
                let since = self.lands_since(&signed, base)?;
                ReceiptState::Reusable(signed, since.into_iter().map(|(c, _)| c).collect())
            }
            (Verdict::Land, Some(signed)) => ReceiptState::Valid(signed),
            _ => ReceiptState::Unbound,
        })
    }

    /// The lands after the receipt's base, through `base`.
    fn lands_since(
        &self,
        signed: &SignedReceipt,
        base: &CommitId,
    ) -> LandResult<Vec<(CommitId, Tier)>> {
        let base_ref = &self.config.base_ref;
        let Some(seq) = self.ledger.base_seq(base_ref, &signed.receipt.base)? else {
            return Ok(Vec::new());
        };
        self.ledger.lands_between(base_ref, seq, base)
    }

    /// The lands since the receipt whose reach meets the change's. A stack
    /// whose ancestors moved since its receipt needs a new one whatever the
    /// reach, so then every non-prose land is named.
    fn intersecting(
        &self,
        top: &ChangeFacts,
        base: &CommitId,
        tree: &TreeId,
        since: &[(CommitId, Tier)],
    ) -> LandResult<Vec<CommitId>> {
        let found = match &self.recheck_store {
            Some(store) => reach_store::intersecting(
                store.as_ref(),
                &self.units_cache,
                base,
                top,
                tree,
                since,
            )?,
            None => Vec::new(),
        };
        if !found.is_empty() {
            return Ok(found);
        }
        Ok(since
            .iter()
            .filter(|(_, tier)| *tier != Tier::ProseOnly)
            .map(|(commit, _)| *commit)
            .collect())
    }

    /// The units the change reaches: from the base to its merge result, as
    /// reuse measures it, or from the stack's root to its head when it does
    /// not merge.
    fn reach_of(
        &self,
        stack: &[ChangeFacts],
        top: &ChangeFacts,
        base: &CommitId,
        merge: &MergeCheck,
    ) -> LandResult<UnitsReached> {
        let prose = top.tier == Tier::ProseOnly;
        let Some(store) = &self.recheck_store else {
            return Ok(if prose {
                UnitsReached::Units(Vec::new())
            } else {
                UnitsReached::All
            });
        };
        let ends = match merge {
            MergeCheck::Clean(tree) => [base, tree],
            _ => [&stack[0].merge_base, &top.head],
        };
        reach_store::units_reached(store.as_ref(), &self.units_cache, base, ends, prose)
    }
}

fn approval_check(top: &ChangeFacts, review: ReviewMode) -> ApprovalCheck {
    if !review.approval_gates_land() {
        ApprovalCheck::NotRequired
    } else if top.approved_head == Some(top.head) {
        ApprovalCheck::Held
    } else {
        ApprovalCheck::Needed
    }
}

/// The refusal a land attempt reaches first: a layer's gate, then the
/// merge and size checks, then the rule on the gathered inputs.
fn first_refusal(
    layers: &[Option<Refusal>],
    top: &ChangeId,
    review: ReviewMode,
    merge: &MergeCheck,
    gathered: Option<&Gathered>,
) -> Option<CheckRefusal> {
    let last = layers.len() - 1;
    if let Some(layer) = layers.iter().position(Option::is_some) {
        let refusal = match layers[layer] {
            Some(refusal) if layer == last => refusal,
            _ => Refusal::ParentUnlanded,
        };
        return Some(CheckRefusal::Rule(refusal));
    }
    let refusal = match merge {
        // As the land: the top's own conflict, or a lower layer's.
        MergeCheck::Conflicted { change, .. } if change == top => {
            CheckRefusal::Rule(Refusal::MergeConflict)
        }
        MergeCheck::Conflicted { .. } => CheckRefusal::Rule(Refusal::ParentUnlanded),
        MergeCheck::Regenerate {
            auto_at_land: true, ..
        } if review == ReviewMode::AfterLand => return None,
        MergeCheck::Regenerate {
            change,
            paths,
            merged_tree,
            commands,
            ..
        } => CheckRefusal::Regenerate {
            change: *change,
            paths: paths.clone(),
            merged_tree: *merged_tree,
            commands: commands.clone(),
        },
        MergeCheck::Oversize { limit, files } => CheckRefusal::Oversize {
            limit: *limit,
            files: files.clone(),
        },
        MergeCheck::Clean(_) | MergeCheck::StoredConflicts | MergeCheck::NotChecked => {
            match gathered.map(|g| rules::decide(&g.input)) {
                Some(Verdict::Refuse(refusal)) => CheckRefusal::Rule(refusal),
                _ => return None,
            }
        }
    };
    Some(refusal)
}
