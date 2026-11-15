//! Check §5's landing evidence against immutable history, not just the
//! existence of some receipt or approval for the change (#173).

use super::{is_server_revert, landed, Failures, Run};
use crate::plan::Plan;
use crate::report::ReceiptSample;
use anyhow::{bail, Context, Result};
use rosen_client::protocol::{Approval, LogEntry, ReadAnswer, ReadRequest};
use rosen_client::{ChangeId, Client, ObjectId};
use rosen_core::receipt::PRE_LAND_CHECK_SET;
use std::collections::{BTreeMap, HashMap, HashSet};

type Root = BTreeMap<String, (String, ObjectId)>;

struct Gates<'a, 'r> {
    client: &'a Client,
    run: &'a Run<'r>,
    history: HashMap<ObjectId, &'a LogEntry>,
    units: HashMap<ChangeId, usize>,
    reviewers: HashSet<String>,
    roots: HashMap<ObjectId, Root>,
}

pub(super) async fn check(client: &Client, run: &Run<'_>, failures: &mut Failures) -> Result<()> {
    let mut gates = Gates {
        client,
        run,
        history: run.history.iter().map(|e| (e.commit, e)).collect(),
        units: landed(run.reports)
            .map(|c| (c.change, run.plan.unit_of(c.index)))
            .collect(),
        reviewers: run
            .reports
            .iter()
            .map(|r| run.plan.reviewer_name(&r.machine))
            .collect(),
        roots: HashMap::new(),
    };
    for entry in run.history.iter().filter(|e| !is_server_revert(e)) {
        gates.check_entry(entry, failures).await?;
    }
    Ok(())
}

impl Gates<'_, '_> {
    async fn check_entry(&mut self, entry: &LogEntry, failures: &mut Failures) -> Result<()> {
        let Some(change) = entry.change else {
            return Ok(()); // check_history reports commits without a change.
        };
        match self.matching_receipt(entry) {
            None => failures.push(format!(
                "{change} landed as {} with a receipt trailer the run never gave",
                entry.commit
            )),
            Some(sample) if sample.tree.is_none() => failures.push(format!(
                "{change}: receipt lacks checked-tree evidence; older reports need a new run"
            )),
            Some(sample) if self.covers(&sample, entry).await? => {}
            _ => failures.push(format!(
                "{change} landed as {} with a tree its run receipt did not cover",
                entry.commit
            )),
        }
        if !self.has_approval(entry).await? {
            failures.push(format!(
                "{change} landed as {} without a trailer naming a run approval covering its landed tree",
                entry.commit
            ));
        }
        Ok(())
    }

    fn matching_receipt(&self, entry: &LogEntry) -> Option<ReceiptSample> {
        self.run
            .reports
            .iter()
            .flat_map(|r| &r.receipts)
            .find(|sample| {
                Some(sample.change) == entry.change
                    && entry.receipt.as_ref().is_some_and(|trailer| {
                        trailer.check_set() == PRE_LAND_CHECK_SET
                            && trailer.head == Some(sample.head)
                            && trailer.base == sample.base
                            && sample.submitter.as_ref() == Some(&trailer.submitter)
                    })
            })
            .cloned()
    }

    async fn has_approval(&mut self, entry: &LogEntry) -> Result<bool> {
        let Some(change) = entry.change else {
            return Ok(false);
        };
        let approvals = self
            .client
            .review_approvals(&change.to_string())
            .await
            .context("reading approvals")?;
        let approvals: Vec<_> = approvals
            .into_iter()
            .filter(|a| self.named_approval(a, entry))
            .collect();
        for approval in approvals {
            let samples: Vec<_> = self
                .run
                .reports
                .iter()
                .flat_map(|r| &r.receipts)
                .filter(|s| s.change == change && s.head == approval.head)
                .cloned()
                .collect();
            for sample in samples {
                if self.covers(&sample, entry).await? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn named_approval(&self, approval: &Approval, entry: &LogEntry) -> bool {
        // R12 voids approvals when reopening the culprit. Judge that historical
        // land using its trailers and original heads, not its reopened state.
        let reverted = self
            .run
            .culprit
            .is_some_and(|c| Some(c.change) == entry.change)
            && self.run.history.iter().any(|revert| {
                is_server_revert(revert) && revert.summary == format!("Revert {}", entry.commit)
            });
        let identity = approval
            .machine
            .as_ref()
            .and_then(|machine| approval.reviewer.on_machine(machine).ok());
        Some(approval.change) == entry.change
            && (approval.valid || reverted)
            && approval.created_at.div_euclid(1000) <= entry.time
            && self.reviewers.contains(approval.reviewer.as_str())
            && identity.is_some_and(|reviewer| entry.approvers.contains(&reviewer))
    }

    async fn covers(&mut self, sample: &ReceiptSample, entry: &LogEntry) -> Result<bool> {
        let (Some(tree), [parent]) = (sample.tree, entry.parents.as_slice()) else {
            return Ok(false);
        };
        let Some(&unit) = self.units.get(&sample.change) else {
            return Ok(false);
        };
        if !self.base_precedes(sample.base, *parent, None) {
            return Ok(false);
        }
        if tree == entry.tree {
            return Ok(true); // R8 permits exact checked-tree reuse across rebases.
        }
        if !self.base_precedes(sample.base, *parent, Some(unit)) {
            return Ok(false);
        }
        // The seed has one independent build unit per root directory. Reuse
        // may import other units from the new parent, but every byte of this
        // unit must still be checked, and the land may change nothing else.
        let checked = self.root(tree).await?;
        let mut expected = self.root(*parent).await?;
        let dir = Plan::unit_dir(unit);
        let Some(checked_unit) = checked.get(&dir) else {
            return Ok(false);
        };
        expected.insert(dir, checked_unit.clone());
        Ok(expected == self.root(entry.tree).await?)
    }

    fn base_precedes(&self, base: ObjectId, mut at: ObjectId, independent: Option<usize>) -> bool {
        while at != base {
            let Some(entry) = self.history.get(&at) else {
                return false;
            };
            if let Some(unit) = independent {
                let changed_unit = if is_server_revert(entry) {
                    self.run.culprit.map(|c| self.run.plan.unit_of(c.index))
                } else {
                    entry.change.and_then(|c| self.units.get(&c).copied())
                };
                if changed_unit.is_none_or(|changed| changed == unit) {
                    return false;
                }
            }
            let [parent] = entry.parents.as_slice() else {
                return false;
            };
            at = *parent;
        }
        true
    }

    async fn root(&mut self, at: ObjectId) -> Result<Root> {
        if let Some(root) = self.roots.get(&at) {
            return Ok(root.clone());
        }
        let request = ReadRequest {
            at: Some(at.to_string()),
            path: String::new(),
        };
        let ReadAnswer::Directory { entries, .. } = self
            .client
            .read_file(request)
            .await
            .with_context(|| format!("reading landing evidence tree {at}"))?
        else {
            bail!("landing evidence {at} is not a tree");
        };
        let root: Root = entries
            .into_iter()
            .map(|e| (e.name, (e.mode, e.id)))
            .collect();
        self.roots.insert(at, root.clone());
        Ok(root)
    }
}
