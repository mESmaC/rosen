//! Capture concurrent intent and durable edits inside the claim transaction.
//! Unlike the overlap query, notices include generated and registration paths.
use crate::changes::{self, ChangeError};
use rosen_core::{ChangeId, ChangeParent, ChangeState, ObjectId, PathPattern};
use rosen_protocol::{ChangeFilter, ChangeRecord, ClaimKind, OverlapRecord};
use rosen_store::ObjectStore;
use rusqlite::Transaction;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(super) fn find(
    tx: &Transaction,
    store: &dyn ObjectStore,
    claimant: ChangeId,
    patterns: &[PathPattern],
) -> Result<Vec<OverlapRecord>, ChangeError> {
    let records = live_changes(tx)?;
    let heads = parent_heads(tx, &records)?;
    let mut paths: BTreeMap<ChangeId, BTreeSet<String>> = BTreeMap::new();
    for record in super::store::records(tx)? {
        if record.change != claimant && record.kind != ClaimKind::Wait {
            let held = PathPattern::new(&record.path)?;
            if patterns.iter().any(|p| p.overlaps(&held)) {
                paths.entry(record.change).or_default().insert(record.path);
            }
        }
    }
    for record in &records {
        let change = &record.change;
        if change.id == claimant || change.state.is_closed() {
            continue;
        }
        // A change with no snapshots has no authored edits, even after a rebase.
        if change.snapshots.is_empty() {
            continue;
        }
        let changed = crate::overlaps::edits::changed_files(store, &heads, change)
            .map_err(|err| ChangeError::Base(err.to_string()))?;
        for path in changed.into_keys() {
            if patterns.iter().any(|p| p.matches(&path)) {
                paths.entry(change.id).or_default().insert(path);
            }
        }
    }
    Ok(records
        .into_iter()
        .filter_map(|record| {
            paths.remove(&record.change.id).map(|paths| OverlapRecord {
                change: record.change.id,
                owner: record.change.owner,
                paths: paths.into_iter().collect(),
            })
        })
        .collect())
}

fn live_changes(tx: &Transaction) -> Result<Vec<ChangeRecord>, ChangeError> {
    let mut records = Vec::new();
    for state in [ChangeState::Open, ChangeState::Ready] {
        records.extend(changes::list(
            tx,
            &ChangeFilter {
                state: Some(state),
                owner: None,
            },
        )?);
    }
    records.sort_by_key(|record| record.change.id);
    Ok(records)
}

fn parent_heads(
    tx: &Transaction,
    records: &[ChangeRecord],
) -> Result<HashMap<ChangeId, ObjectId>, ChangeError> {
    let mut heads: HashMap<_, _> = records
        .iter()
        .map(|r| (r.change.id, r.change.head))
        .collect();
    for record in records {
        if let ChangeParent::Change(parent) = record.change.parent {
            if let std::collections::hash_map::Entry::Vacant(slot) = heads.entry(parent) {
                slot.insert(changes::get(tx, parent)?.change.head);
            }
        }
    }
    Ok(heads)
}
