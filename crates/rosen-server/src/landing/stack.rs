//! One R8 request prepares a bottom-first commit chain and moves the ref once.
use super::*;
use rosen_core::ChangeState;

impl Landing {
    fn stack_facts(&self, top: &ChangeId) -> LandResult<Vec<ChangeFacts>> {
        let mut stack = Vec::new();
        let mut id = *top;
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(id) {
                return Err(LandError::Backend("cycle in stack".into()));
            }
            let facts = self.facts(&id)?;
            let parent = facts.parent;
            stack.push(facts);
            match parent {
                ChangeParent::Base(_) => break,
                ChangeParent::Change(parent) if self.ledger.landing_of(&parent)?.is_some() => break,
                ChangeParent::Change(parent) => id = parent,
            }
        }
        stack.reverse();
        Ok(stack)
    }

    pub(super) fn preview_stack(
        &self,
        change: &ChangeId,
        base: &CommitId,
    ) -> LandResult<MergeOutcome> {
        let stack = self.stack_facts(change)?;
        let mut onto = *base;
        for facts in stack {
            match self
                .deps
                .merger
                .merge_layer(&facts.merge_base, &onto, &facts.head, base)?
            {
                MergeOutcome::Clean(tree) => onto = tree,
                conflict => return Ok(conflict),
            }
        }
        Ok(MergeOutcome::Clean(onto))
    }

    pub(super) fn attempt_stack(&self, change: &ChangeId, attempts: u32) -> LandResult<Attempt> {
        let mut timing = super::super::timing::Timing::new(change, "attempt");
        let base = self.current_base()?;
        let policy = self.policy_at(&base)?;
        let mut stack = self.stack_facts(change)?;
        timing.phase("facts");
        if let Some(attempt) = self.check_layers(change, &base, policy.review, &mut stack)? {
            return Ok(attempt);
        }
        timing.phase("tiers");
        let Some(trees) = self.merge_layers(&stack, base)? else {
            return Ok(Attempt::Retry);
        };
        timing.phase("merge");
        if trees.len() != stack.len() {
            let reason = if trees.len() + 1 == stack.len() {
                Refusal::MergeConflict
            } else {
                Refusal::ParentUnlanded
            };
            return Ok(Attempt::Done(LandOutcome::Refused(reason)));
        }
        if self.current_base()? != base {
            return Ok(Attempt::Retry);
        }
        self.check_file_sizes(&base, &trees)?;
        timing.phase("sizes");
        let tree = *trees.last().expect("one tree per layer");
        let Some(gathered) = self.gather_stack(&stack, tree, &base, &policy)? else {
            return Ok(Attempt::Retry);
        };
        timing.phase("receipt");
        if let Verdict::Refuse(reason) = rules::decide(&gathered.input) {
            return Ok(Attempt::Done(refused(Verdict::Refuse(reason))));
        }
        // Merges and receipt reach can outlive another land. Avoid scanning
        // and durably writing a commit whose CAS is already certain to lose.
        // The transaction still rechecks the ref and every mutable input.
        if self.current_base()? != base {
            return Ok(Attempt::Retry);
        }
        let result = self.commit_stack(
            &stack,
            &trees,
            base,
            gathered.receipt,
            policy.review,
            attempts,
        );
        timing.phase("write_and_cas");
        result
    }

    fn gather_stack(
        &self,
        stack: &[ChangeFacts],
        tree: TreeId,
        base: &CommitId,
        policy: &LandPolicy,
    ) -> LandResult<Option<Gathered>> {
        let mut top = stack.last().expect("nonempty stack").clone();
        // Code anywhere in the stack requires the top's receipt, even if
        // the top layer itself only edits prose.
        if stack.iter().any(|f| f.tier != Tier::ProseOnly) && top.tier == Tier::ProseOnly {
            top.tier = Tier::Normal;
        }
        self.gather(&top, base, policy, tree, stack.len() == 1)
    }

    fn check_layers(
        &self,
        change: &ChangeId,
        base: &CommitId,
        review: ReviewMode,
        stack: &mut [ChangeFacts],
    ) -> LandResult<Option<Attempt>> {
        for facts in stack {
            facts.tier = self.deps.book.tier_at(facts, base)?;
            if let Some(refusal) = layer_refusal(facts, review) {
                if self.current_base()? != *base {
                    return Ok(Some(Attempt::Retry));
                }
                let reason = if facts.id == *change {
                    refusal
                } else {
                    Refusal::ParentUnlanded
                };
                return Ok(Some(Attempt::Done(LandOutcome::Refused(reason))));
            }
        }
        Ok(None)
    }

    fn check_file_sizes(&self, base: &CommitId, trees: &[TreeId]) -> LandResult<()> {
        if let Some(store) = &self.recheck_store {
            let policy = crate::project_config::resolve(store.as_ref(), base)
                .map_err(|e| LandError::Backend(e.to_string()))?;
            super::super::size_limit::check(store.as_ref(), trees, policy.config.max_file_size)?;
        }
        Ok(())
    }

    /// Route a conflict to the layer that produced it. Never land a prefix
    /// when a higher layer fails; the request is all or nothing.
    fn merge_layers(
        &self,
        stack: &[ChangeFacts],
        base: CommitId,
    ) -> LandResult<Option<Vec<TreeId>>> {
        let mut trees = Vec::new();
        let mut onto = base;
        for facts in stack {
            // A live attempt can discard stale work; historical receipt
            // previews use `preview_stack` and keep their explicitly named base.
            if self.current_base()? != base {
                return Ok(None);
            }
            let Some(outcome) = self
                .deps
                .merger
                .merge_layer_live([&facts.merge_base, &onto, &facts.head, &base], &|| {
                    self.current_base().map(|current| current == base)
                })?
            else {
                return Ok(None);
            };
            match outcome {
                MergeOutcome::Clean(tree) => {
                    trees.push(tree);
                    onto = tree;
                }
                MergeOutcome::Conflicted(paths) => {
                    let routing = self.deps.book.record_conflicts_at(
                        &facts.id,
                        &facts.merge_base,
                        &onto,
                        &facts.head,
                        &paths,
                    )?;
                    if routing == Routing::ChangeMoved {
                        return Ok(None);
                    }
                    self.deps.events.emit(LandingEvent::Conflict {
                        change: facts.id,
                        paths,
                    });
                    break;
                }
            }
        }
        Ok(Some(trees))
    }

    fn commit_stack(
        &self,
        stack: &[ChangeFacts],
        trees: &[TreeId],
        base: CommitId,
        receipt: Option<SignedReceipt>,
        review: ReviewMode,
        attempts: u32,
    ) -> LandResult<Attempt> {
        let mut timing =
            super::super::timing::Timing::new(stack.last().expect("nonempty stack").id, "commit");
        let mut records = Vec::new();
        let mut parent = base;
        for (facts, tree) in stack.iter().zip(trees) {
            let landed = landed_commit(facts, parent, *tree, receipt.as_ref())?;
            let commit = self.deps.writer.write_landed(&landed)?;
            records.push(LandRecord {
                ref_name: self.config.base_ref.clone(),
                change: facts.id,
                head: facts.head,
                commit,
                tier: facts.tier,
            });
            parent = commit;
        }
        timing.phase("write");
        let swapped =
            self.ledger
                .compare_stack(&base, &records, stack, receipt.as_ref(), review)?;
        timing.phase("cas");
        match swapped {
            CasResult::Swapped { .. } => {
                let record = records.pop().expect("nonempty stack");
                Ok(Attempt::Committed(CommittedLand {
                    record,
                    ancestors: records,
                    attempts,
                }))
            }
            CasResult::BaseMoved { .. }
            | CasResult::ChangeMoved
            | CasResult::ReceiptUntrusted
            | CasResult::AlreadyLanded { .. } => Ok(Attempt::Retry),
            CasResult::OpenConflicts => Ok(Attempt::Done(LandOutcome::Refused(Refusal::Conflicts))),
        }
    }
}

fn layer_refusal(facts: &ChangeFacts, review: ReviewMode) -> Option<Refusal> {
    match facts.state {
        ChangeState::Landed => return Some(Refusal::AlreadyLanded),
        ChangeState::Abandoned => return Some(Refusal::Abandoned),
        _ => {}
    }
    if facts.has_conflicts {
        return Some(Refusal::Conflicts);
    }
    if review.approval_gates_land() && facts.approved_head != Some(facts.head) {
        return Some(Refusal::Unapproved);
    }
    None
}
