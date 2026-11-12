//! Stacks (SPEC.md §2, D19): chains of changes, each the parent of the next.
//! A stack lands bottom first, and when a change lands or updates, the
//! changes directly on top of it are the ones the server rebases.

use crate::change::{ChangeId, ChangeParent};
use crate::hash::ObjectId;
use crate::{Error, Result};
use std::collections::{HashMap, HashSet};

/// The chain from a stack's bottom change up to a chosen top change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// The base commit the bottom change sits on.
    pub base: ObjectId,
    /// Bottom first: the order the stack must land in.
    pub changes: Vec<ChangeId>,
}

impl Stack {
    /// Follows parent links down from `top` to a base commit. `parents` maps
    /// each unlanded change to its parent; a missing entry or a cycle is an
    /// error, because either would let a change land before its parent.
    pub fn resolve(top: ChangeId, parents: &HashMap<ChangeId, ChangeParent>) -> Result<Self> {
        let mut changes = vec![top];
        let mut seen = HashSet::from([top]);
        let mut current = top;
        loop {
            let parent = parents.get(&current).ok_or(Error::UnknownChange(current))?;
            match *parent {
                ChangeParent::Base(base) => {
                    changes.reverse();
                    return Ok(Stack { base, changes });
                }
                ChangeParent::Change(below) => {
                    if !seen.insert(below) {
                        return Err(Error::StackCycle(below));
                    }
                    changes.push(below);
                    current = below;
                }
            }
        }
    }

    pub fn bottom(&self) -> ChangeId {
        self.changes[0]
    }

    pub fn top(&self) -> ChangeId {
        *self
            .changes
            .last()
            .expect("a stack holds at least one change")
    }

    /// Only the bottom change may land next (R8 step 3).
    pub fn may_land(&self, change: ChangeId) -> bool {
        self.bottom() == change
    }
}

/// The changes stacked directly on `change`, in ID order so the server
/// rebases them deterministically.
pub fn dependents(change: ChangeId, parents: &HashMap<ChangeId, ChangeParent>) -> Vec<ChangeId> {
    let mut above: Vec<ChangeId> = parents
        .iter()
        .filter(|(_, parent)| **parent == ChangeParent::Change(change))
        .map(|(id, _)| *id)
        .collect();
    above.sort();
    above
}

/// Orders a set of changes so every change comes after its parent change,
/// for the server to rebase or land a whole forest of stacks. Each stack is
/// emitted whole, and stacks and siblings follow ID order.
pub fn landing_order(parents: &HashMap<ChangeId, ChangeParent>) -> Result<Vec<ChangeId>> {
    let mut children: HashMap<ChangeId, Vec<ChangeId>> = HashMap::new();
    let mut pending: Vec<ChangeId> = Vec::new();
    for (id, parent) in parents {
        match parent {
            ChangeParent::Base(_) => pending.push(*id),
            ChangeParent::Change(below) => children.entry(*below).or_default().push(*id),
        }
    }
    // Popping from the end, so sort descending to take the lowest ID first.
    pending.sort_by(|a, b| b.cmp(a));
    let mut order = Vec::with_capacity(parents.len());
    while let Some(next) = pending.pop() {
        order.push(next);
        if let Some(above) = children.get_mut(&next) {
            above.sort_by(|a, b| b.cmp(a));
            pending.append(above);
        }
    }
    if order.len() < parents.len() {
        let placed: HashSet<ChangeId> = order.iter().copied().collect();
        let stranded = parents.keys().filter(|id| !placed.contains(*id)).min();
        // Anything left over never reached a base: its chain is cyclic or
        // names a change outside the set.
        let stranded = *stranded.expect("fewer placed than given");
        return Err(match Stack::resolve(stranded, parents) {
            Err(error) => error,
            Ok(_) => Error::StackCycle(stranded),
        });
    }
    Ok(order)
}
