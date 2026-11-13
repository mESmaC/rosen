//! Post-land work (SPEC R8 step 4, D19): release claims, then rebase the
//! changes stacked on the landed change and the changes that overlap it onto
//! the new base. Every other change rebases only when it lands itself.
//!
//! Every step is idempotent, so the engine can replay it after a crash.

use super::{
    ChangeBook, ChangeId, ChangeState, CommitId, EventSink, LandError, LandResult, LandingEvent,
    MergeOutcome, Merger, Routing,
};

/// How often one target is re-merged when snapshots keep moving its head
/// while its conflicts are being stored. Each round re-reads the new head,
/// so only a change edited continuously through every round exhausts it.
const ROUTING_ROUNDS: u32 = 8;

/// What the post-land rebase did, for callers and logs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestackReport {
    pub rebased: Vec<ChangeId>,
    pub conflicted: Vec<ChangeId>,
}

pub(crate) struct Restacker<'a> {
    pub book: &'a dyn ChangeBook,
    pub merger: &'a dyn Merger,
    pub events: &'a dyn EventSink,
}

impl Restacker<'_> {
    pub fn after_land(&self, landed: &ChangeId, commit: &CommitId) -> LandResult<RestackReport> {
        // Capture durably before closing the subject or deleting its claims.
        let targets = self.targets(landed)?;
        self.book.mark_landed(landed, commit)?;
        self.book.release_claims(landed)?;
        self.events.emit(LandingEvent::Released { change: *landed });
        let mut report = RestackReport::default();
        for target in targets {
            self.rebase_one(&target, commit, &mut report)?;
        }
        Ok(report)
    }

    /// Stacked changes first, in stack order, then overlapping changes in ID
    /// order. Order is only set between changes that are stacked or overlap.
    fn targets(&self, landed: &ChangeId) -> LandResult<Vec<ChangeId>> {
        let mut targets = self.book.stacked_on(landed)?;
        let mut overlapping = self.book.overlapping(landed)?;
        overlapping.sort();
        for change in overlapping {
            if change != *landed && !targets.contains(&change) {
                targets.push(change);
            }
        }
        Ok(targets)
    }

    fn rebase_one(
        &self,
        change: &ChangeId,
        onto: &CommitId,
        report: &mut RestackReport,
    ) -> LandResult<()> {
        for _ in 0..ROUTING_ROUNDS {
            if self.try_rebase(change, onto, report)? {
                return Ok(());
            }
        }
        Err(LandError::Backend(format!(
            "change {change} kept moving while its rebase conflicts were stored; \
             recovery retries it"
        )))
    }

    /// One rebase of `change` onto `onto`. Returns false when a snapshot
    /// moved its head before conflicts were stored: nothing from the stale
    /// head is attached, and the caller merges again.
    fn try_rebase(
        &self,
        change: &ChangeId,
        onto: &CommitId,
        report: &mut RestackReport,
    ) -> LandResult<bool> {
        let Some(facts) = self.book.facts(change)? else {
            return Ok(true);
        };
        if matches!(facts.state, ChangeState::Landed | ChangeState::Abandoned) {
            return Ok(true);
        }
        match self.merger.merge(&facts.merge_base, onto, &facts.head)? {
            MergeOutcome::Clean(tree) => {
                self.book.reparent(change, onto, Some(&tree))?;
                self.events.emit(LandingEvent::Rebased {
                    change: *change,
                    onto: *onto,
                });
                report.rebased.push(*change);
            }
            MergeOutcome::Conflicted(paths) => {
                // Conflicts are stored on the change, never raised as errors
                // (D2): the change moves anyway and its owner resolves them.
                self.book.reparent(change, onto, None)?;
                let routing = self.book.record_conflicts_at(
                    change,
                    &facts.merge_base,
                    onto,
                    &facts.head,
                    &paths,
                )?;
                if routing == Routing::ChangeMoved {
                    return Ok(false);
                }
                self.events.emit(LandingEvent::Conflict {
                    change: *change,
                    paths,
                });
                report.conflicted.push(*change);
            }
        }
        Ok(true)
    }
}
