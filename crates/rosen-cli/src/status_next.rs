//! Next-action synthesis for `rosen status` (SPEC R10): turns raw change,
//! workspace and land-check state into an explicit `next_action`, responsible
//! `actor`, `affected_head` and stored-conflict summary.

use rosen_client::protocol::{ChangeRecord, LandCheck, MergeGate};
use rosen_core::{ChangeParent, ChangeState, ObjectId, SnapshotKind};
use serde::Serialize;

use crate::status_cmd::WorkspaceState;

/// Who acts next on the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Agent,
    Velocity,
    Owner,
}

impl Actor {
    pub fn as_str(self) -> &'static str {
        match self {
            Actor::Agent => "agent",
            Actor::Velocity => "velocity",
            Actor::Owner => "owner",
        }
    }
}

/// Structured details for open stored conflicts on a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConflictState {
    /// Repository-relative paths with open stored conflicts.
    pub paths: Vec<String>,
    /// True when the workspace already holds the rebased head on the current
    /// base; false when `rosen rebase` must run first; `None` when no local
    /// workspace was scanned.
    pub base_applied: Option<bool>,
    /// True when the change was previously named (or marked ready), so after
    /// resolving conflicts it must be named again with `rosen commit -m`.
    pub needs_naming_again: bool,
}

/// The synthesized next step for `rosen status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NextStep {
    pub next_action: String,
    pub actor: Option<Actor>,
    pub affected_head: ObjectId,
    pub conflict: Option<ConflictState>,
}

/// Sorted, deduplicated paths of stored conflicts from a land check.
pub fn stored_conflict_paths(gates: Option<&LandCheck>) -> Vec<String> {
    let Some(LandCheck {
        merge: MergeGate::Conflicts { conflicts },
        ..
    }) = gates
    else {
        return Vec::new();
    };
    let mut paths: Vec<String> = conflicts.iter().map(|c| c.path.clone()).collect();
    paths.sort();
    paths.dedup();
    paths
}

/// True when the change was already named with `rosen commit -m` or marked ready.
pub fn was_named(record: &ChangeRecord) -> bool {
    record.change.state == ChangeState::Ready
        || record
            .change
            .snapshots
            .iter()
            .any(|s| matches!(s.kind, SnapshotKind::Named(_)))
}

/// Derives `next_action`, `actor`, `affected_head` and stored-conflict state.
pub fn derive(
    record: &ChangeRecord,
    gates: Option<&LandCheck>,
    local: Option<&WorkspaceState>,
    managed: bool,
    current_agent: Option<&str>,
) -> NextStep {
    let change = &record.change;
    if matches!(change.state, ChangeState::Landed | ChangeState::Abandoned) {
        return step("none", None, change.head, None);
    }
    let change_actor = actor_for_change(record, local, current_agent);
    if has_stored_conflicts(record, gates) {
        return stored_conflict_step(record, gates, local, change_actor);
    }
    if let Some(ws) = local {
        if ws.journal_pending > 0 {
            return step("sync_journal", Some(change_actor), ws.stored_tree, None);
        }
        if ws.rebase_waiting {
            return step("apply_rebase", Some(change_actor), change.head, None);
        }
    }
    if let Some(blocked) = gates.and_then(|g| merge_or_stack_step(record, g, change_actor)) {
        return blocked;
    }
    if let Some(local_step) = unnamed_or_dirty_step(record, local, change_actor) {
        return local_step;
    }
    let landing_actor = if managed {
        Actor::Velocity
    } else {
        Actor::Owner
    };
    gate_step(record, gates, landing_actor)
}

fn actor_for_change(
    record: &ChangeRecord,
    local: Option<&WorkspaceState>,
    current_agent: Option<&str>,
) -> Actor {
    if local.is_some() {
        return Actor::Agent;
    }
    match current_agent {
        Some(agent) if agent != record.change.owner.as_str() => Actor::Owner,
        _ => Actor::Agent,
    }
}

fn has_stored_conflicts(record: &ChangeRecord, gates: Option<&LandCheck>) -> bool {
    !record.change.conflicts.is_empty()
        || matches!(gates.map(|g| &g.merge), Some(MergeGate::Conflicts { .. }))
}

fn stored_conflict_step(
    record: &ChangeRecord,
    gates: Option<&LandCheck>,
    local: Option<&WorkspaceState>,
    change_actor: Actor,
) -> NextStep {
    let change = &record.change;
    let base_behind = match (&change.parent, gates) {
        (ChangeParent::Base(parent_base), Some(g)) => *parent_base != g.base,
        _ => false,
    };
    let base_applied = local.map(|ws| !ws.rebase_waiting && !base_behind);
    let conflict = ConflictState {
        paths: stored_conflict_paths(gates),
        base_applied,
        needs_naming_again: was_named(record),
    };
    let action = if base_applied == Some(false) {
        "rebase_and_resolve_conflicts"
    } else {
        "resolve_conflicts"
    };
    step(action, Some(change_actor), change.head, Some(conflict))
}

fn merge_or_stack_step(
    record: &ChangeRecord,
    gates: &LandCheck,
    change_actor: Actor,
) -> Option<NextStep> {
    let change = &record.change;
    let parent_unlanded = gates
        .refusal
        .as_ref()
        .is_some_and(|r| r.reason == "parent_unlanded");
    // A blocked ancestor fails before any merge or regeneration step in a
    // same-base land, so point to it first; lower-layer merge conflicts have
    // no blocked stack entry and fall through to merge routing below (#474).
    if parent_unlanded {
        let next = gates
            .stack
            .iter()
            .find(|layer| layer.blocked.is_some())
            .map(|layer| step("unblock_parent", Some(Actor::Owner), layer.head, None))
            .or_else(|| merge_gate_step(change, gates, change_actor))
            .unwrap_or_else(|| step("unblock_parent", Some(Actor::Owner), change.head, None));
        return Some(next);
    }
    merge_gate_step(change, gates, change_actor)
}

fn merge_gate_step(
    change: &rosen_core::Change,
    gates: &LandCheck,
    change_actor: Actor,
) -> Option<NextStep> {
    match &gates.merge {
        MergeGate::MergeConflict { change: target, .. } => {
            if *target == change.id {
                return Some(step(
                    "rebase_and_resolve_conflicts",
                    Some(change_actor),
                    change.head,
                    None,
                ));
            }
            let head = ancestor_head(gates, *target, change.head);
            Some(step("unblock_parent", Some(Actor::Owner), head, None))
        }
        MergeGate::ConflictMarkers { .. } => Some(step(
            "remove_conflict_markers",
            Some(change_actor),
            change.head,
            None,
        )),
        MergeGate::RegenerationRequired {
            change: target,
            pending_at_land: false,
            ..
        } => {
            if *target == change.id {
                return Some(step(
                    "regenerate_files",
                    Some(change_actor),
                    change.head,
                    None,
                ));
            }
            let head = ancestor_head(gates, *target, change.head);
            Some(step("unblock_parent", Some(Actor::Owner), head, None))
        }
        MergeGate::OversizeFiles { .. } => Some(step(
            "fix_oversize_files",
            Some(change_actor),
            change.head,
            None,
        )),
        MergeGate::Clean
        | MergeGate::Conflicts { .. }
        | MergeGate::RegenerationRequired {
            pending_at_land: true,
            ..
        }
        | MergeGate::NotChecked => None,
    }
}

fn unnamed_or_dirty_step(
    record: &ChangeRecord,
    local: Option<&WorkspaceState>,
    change_actor: Actor,
) -> Option<NextStep> {
    let change = &record.change;
    let has_unsnapshotted = local.is_some_and(|w| !w.unsnapshotted.is_empty());
    let is_named = change.state == ChangeState::Ready
        || matches!(
            change.snapshots.last().map(|s| &s.kind),
            Some(SnapshotKind::Named(_))
        );
    if !has_unsnapshotted && is_named {
        return None;
    }
    let action = if change.snapshots.is_empty() && !has_unsnapshotted {
        "edit_and_commit"
    } else {
        "commit"
    };
    Some(step(action, Some(change_actor), change.head, None))
}

fn gate_step(record: &ChangeRecord, gates: Option<&LandCheck>, landing_actor: Actor) -> NextStep {
    let Some(gates) = gates else {
        return step("wait", Some(landing_actor), record.change.head, None);
    };
    if gates.landable
        || (gates.refusal.is_none()
            && matches!(
                gates.merge,
                MergeGate::RegenerationRequired {
                    pending_at_land: true,
                    ..
                }
            ))
    {
        return step("land", Some(landing_actor), gates.head, None);
    }
    let action = match gates.refusal.as_ref().map(|r| r.reason.as_str()) {
        Some("unapproved") => "approve",
        Some("no_receipt" | "receipt_stale" | "needs_recheck") => "submit_receipt",
        Some(other) => other,
        None => "wait",
    };
    step(action, Some(landing_actor), gates.head, None)
}

fn ancestor_head(gates: &LandCheck, target: rosen_core::ChangeId, fallback: ObjectId) -> ObjectId {
    gates
        .stack
        .iter()
        .find(|layer| layer.change == target)
        .map_or(fallback, |layer| layer.head)
}

fn step(
    next_action: &str,
    actor: Option<Actor>,
    affected_head: ObjectId,
    conflict: Option<ConflictState>,
) -> NextStep {
    NextStep {
        next_action: next_action.to_owned(),
        actor,
        affected_head,
        conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosen_client::protocol::{
        ApprovalGate, LandReach, LandRefusal, ReceiptGate, StackLayer, StoredConflict,
    };
    use rosen_core::{AgentId, Change, ChangeId, ConflictId, Snapshot};
    use std::path::PathBuf;

    fn oid(ch: char) -> ObjectId {
        ch.to_string().repeat(40).parse().unwrap()
    }

    fn cid(ch: char) -> ChangeId {
        ch.to_string().repeat(32).parse().unwrap()
    }

    fn record(
        state: ChangeState,
        snapshots: Vec<Snapshot>,
        conflicts: Vec<ConflictId>,
    ) -> ChangeRecord {
        let mut change = Change::new(
            cid('k'),
            AgentId::new("agent-1").unwrap(),
            ChangeParent::Base(oid('1')),
            oid('a'),
        );
        change.state = state;
        change.snapshots = snapshots;
        change.conflicts = conflicts;
        ChangeRecord {
            change,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn gates(merge: MergeGate, refusal: Option<&str>, landable: bool) -> LandCheck {
        LandCheck {
            change: cid('k'),
            base_ref: "main".into(),
            base: oid('1'),
            head: oid('a'),
            review_mode: "after-land".into(),
            recheck_mode: "reach".into(),
            tier: "normal".into(),
            reach: LandReach::Units(vec![]),
            merge,
            line_endings: vec![],
            approval: ApprovalGate::NotRequired,
            receipt: ReceiptGate::NotRequired,
            stack: vec![],
            landable,
            refusal: refusal.map(|reason| LandRefusal {
                reason: reason.into(),
                message: String::new(),
                paths: vec![],
                merged_tree: None,
                commands: Default::default(),
                log: None,
            }),
            landed: None,
        }
    }

    fn ws(rebase_waiting: bool) -> WorkspaceState {
        WorkspaceState {
            path: PathBuf::from("ws"),
            stored_tree: oid('a'),
            journal_pending: 0,
            unsnapshotted: vec![],
            rebase_waiting,
        }
    }

    // Risk (R10): wrong next_action or actor misleads agents about whether
    // they, Velocity or an ancestor's owner must act next.
    #[test]
    fn derives_next_action_and_actor() {
        let fresh = record(ChangeState::Open, vec![], vec![]);
        let clean = gates(MergeGate::Clean, None, true);
        let step = derive(
            &fresh,
            Some(&clean),
            Some(&ws(false)),
            true,
            Some("agent-1"),
        );
        assert_eq!(step.next_action, "edit_and_commit");
        assert_eq!(step.actor, Some(Actor::Agent));

        let named = named_record();
        let managed = derive(
            &named,
            Some(&clean),
            Some(&ws(false)),
            true,
            Some("agent-1"),
        );
        assert_eq!(managed.next_action, "land");
        assert_eq!(managed.actor, Some(Actor::Velocity));
        let unmanaged = derive(
            &named,
            Some(&clean),
            Some(&ws(false)),
            false,
            Some("agent-1"),
        );
        assert_eq!(unmanaged.actor, Some(Actor::Owner));

        let mut stacked_gates = gates(MergeGate::Clean, Some("parent_unlanded"), false);
        stacked_gates.stack.push(StackLayer {
            change: cid('m'),
            head: oid('f'),
            state: ChangeState::Open,
            tier: "normal".into(),
            blocked: Some("conflicts".into()),
        });
        let step = derive(&named, Some(&stacked_gates), None, true, Some("agent-1"));
        assert_eq!(step.next_action, "unblock_parent");
        assert_eq!(step.actor, Some(Actor::Owner));
        assert_eq!(step.affected_head, oid('f'));

        let pending_regen = gates(
            MergeGate::RegenerationRequired {
                change: cid('k'),
                paths: vec!["artifact.lock".into()],
                merged_tree: Some(oid('2')),
                commands: [("artifact.lock".into(), "gen".into())].into(),
                pending_at_land: true,
            },
            None,
            false,
        );
        let step = derive(&named, Some(&pending_regen), Some(&ws(false)), true, None);
        assert_eq!(step.next_action, "land");
        assert_eq!(step.actor, Some(Actor::Velocity));

        let manual_regen = gates(
            MergeGate::RegenerationRequired {
                change: cid('k'),
                paths: vec!["artifact.lock".into()],
                merged_tree: Some(oid('2')),
                commands: Default::default(),
                pending_at_land: false,
            },
            Some("regeneration_required"),
            false,
        );
        let step = derive(&named, Some(&manual_regen), Some(&ws(false)), true, None);
        assert_eq!(step.next_action, "regenerate_files");
        assert_eq!(step.actor, Some(Actor::Agent));

        let landed = record(ChangeState::Landed, vec![], vec![]);
        let step = derive(&landed, None, None, true, Some("agent-1"));
        assert_eq!(step.next_action, "none");
        assert_eq!(step.actor, None);
    }

    // Risk (R10): stored-conflict status must name conflicting paths, whether
    // the new base is applied, and whether the change needs naming again.
    #[test]
    fn derives_stored_conflict_details() {
        let conflicted = record(
            ChangeState::Open,
            named_record().change.snapshots,
            vec!["0101010101010101".parse().unwrap()],
        );
        let conflict_gates = gates(
            MergeGate::Conflicts {
                conflicts: vec![StoredConflict {
                    id: "0101010101010101".into(),
                    path: "src/lib.rs".into(),
                }],
            },
            Some("conflicts"),
            false,
        );
        let step = derive(
            &conflicted,
            Some(&conflict_gates),
            Some(&ws(true)),
            true,
            Some("agent-1"),
        );
        assert_eq!(step.next_action, "rebase_and_resolve_conflicts");
        assert_eq!(step.actor, Some(Actor::Agent));
        let info = step.conflict.unwrap();
        assert_eq!(info.paths, vec!["src/lib.rs"]);
        assert_eq!(info.base_applied, Some(false));
        assert!(info.needs_naming_again);
    }

    // Risk (R10): a blocked ancestor refuses a same-base land before child
    // regeneration, while a lower-layer merge conflict without a blocked stack
    // entry still routes to that conflicting ancestor's head (#474).
    #[test]
    fn honors_blocked_parent_before_merge_and_keeps_lower_conflict_routing() {
        let named = named_record();
        let mut blocked_with_regen = gates(
            MergeGate::RegenerationRequired {
                change: cid('k'),
                paths: vec!["artifact.lock".into()],
                merged_tree: Some(oid('2')),
                commands: [("artifact.lock".into(), "gen".into())].into(),
                pending_at_land: false,
            },
            Some("parent_unlanded"),
            false,
        );
        blocked_with_regen.stack.push(StackLayer {
            change: cid('m'),
            head: oid('f'),
            state: ChangeState::Abandoned,
            tier: "normal".into(),
            blocked: Some("abandoned".into()),
        });
        let step = derive(&named, Some(&blocked_with_regen), None, true, None);
        assert_eq!(step.next_action, "unblock_parent");
        assert_eq!(step.actor, Some(Actor::Owner));
        assert_eq!(step.affected_head, oid('f'));

        let mut lower_conflict = gates(
            MergeGate::MergeConflict {
                change: cid('m'),
                paths: vec!["src/lib.rs".into()],
            },
            Some("parent_unlanded"),
            false,
        );
        lower_conflict.stack.push(StackLayer {
            change: cid('m'),
            head: oid('e'),
            state: ChangeState::Ready,
            tier: "normal".into(),
            blocked: None,
        });
        let step = derive(&named, Some(&lower_conflict), None, true, None);
        assert_eq!(step.next_action, "unblock_parent");
        assert_eq!(step.actor, Some(Actor::Owner));
        assert_eq!(step.affected_head, oid('e'));
    }

    fn named_record() -> ChangeRecord {
        record(
            ChangeState::Open,
            vec![Snapshot {
                commit: oid('b'),
                tree: oid('a'),
                kind: SnapshotKind::Named("done".into()),
            }],
            vec![],
        )
    }
}
