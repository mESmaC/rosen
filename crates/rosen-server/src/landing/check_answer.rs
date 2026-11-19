//! The wire answer to R8's land check: the engine's verdicts ([`GateCheck`]),
//! with each refusal worded by the same functions a land's answer uses.

use rosen_core::Change;
use rosen_protocol::{
    ApprovalGate, ErrorBody, LandCheck, LandReach, LandRefusal, LandStatus, LandedSince, MergeGate,
    OversizeFile, ReachAll, ReceiptBinding, ReceiptGate, StackLayer, StoredConflict,
};

use super::reach_store::UnitsReached;
use super::service::{internal, land_body, refusal_message, regeneration_status};
use super::{
    ApprovalCheck, CheckRefusal, GateCheck, LandError, Landing, MergeCheck, ReceiptState, Refusal,
    SignedReceipt,
};
use crate::{changes, conflicts, ServerState};

pub(super) fn answer(
    state: &ServerState,
    engine: &Landing,
    change: &Change,
    gates: GateCheck,
    markers: Vec<String>,
) -> Result<LandCheck, ErrorBody> {
    let stored = match gates.merge {
        MergeCheck::StoredConflicts => stored_conflicts(state, change)?,
        _ => Vec::new(),
    };
    let refusal = refusal(engine, change, &gates, &stored, &markers)?;
    let base_ref = engine.config().base_ref.clone();
    Ok(LandCheck {
        change: change.id,
        base: gates.base,
        head: gates.top.head,
        review_mode: gates.review.as_str().to_owned(),
        recheck_mode: gates.recheck.as_str().to_owned(),
        tier: gates.top.tier.as_str().to_owned(),
        reach: match gates.reach {
            UnitsReached::All => LandReach::All(ReachAll::All),
            UnitsReached::Units(units) => LandReach::Units(units),
        },
        merge: merge_gate(gates.merge, stored, markers),
        // The service adds these from the clean merge result it read.
        line_endings: Vec::new(),
        approval: match gates.approval {
            ApprovalCheck::Held => ApprovalGate::Held {
                reviewers: gates.top.approvers.clone(),
                head: gates.top.head,
            },
            ApprovalCheck::Needed => ApprovalGate::Needed,
            ApprovalCheck::NotRequired => ApprovalGate::NotRequired,
        },
        receipt: receipt_gate(engine, &base_ref, gates.receipt)?,
        stack: gates
            .ancestors
            .iter()
            .map(|(facts, blocked)| StackLayer {
                change: facts.id,
                head: facts.head,
                state: facts.state,
                tier: facts.tier.as_str().to_owned(),
                blocked: blocked.map(|refusal| refusal.code().to_owned()),
            })
            .collect(),
        landable: refusal.is_none(),
        refusal,
        landed: gates.landed,
        base_ref,
    })
}

/// The open conflicts stored on the change, read on a registry read
/// connection.
fn stored_conflicts(
    state: &ServerState,
    change: &Change,
) -> Result<Vec<StoredConflict>, ErrorBody> {
    let load = |conn: &rusqlite::Connection| {
        conflicts::load(conn, &change.id.to_string())
            .map_err(|err| changes::ChangeError::Corrupt(err.to_string()))
    };
    let set = state
        .changes
        .with_read(load)
        .map_err(|err| internal(err.to_string()))?;
    Ok(set
        .open()
        .map(|conflict| StoredConflict {
            id: conflict.id.to_string(),
            path: conflict.path.clone(),
        })
        .collect())
}

/// The refusal the land would answer with on this base. A clean merge
/// result that still holds conflict markers is refused when the land
/// writes its commit, after every other gate has passed.
fn refusal(
    engine: &Landing,
    change: &Change,
    gates: &GateCheck,
    stored: &[StoredConflict],
    markers: &[String],
) -> Result<Option<LandRefusal>, ErrorBody> {
    let status = match &gates.refusal {
        Some(CheckRefusal::Rule(refusal)) => {
            let paths = refusal_paths(*refusal, &gates.merge, stored);
            let message = refusal_message(engine, change, &gates.base, *refusal, &paths)?;
            LandStatus::Refused {
                reason: refusal.code().to_owned(),
                message,
                paths,
            }
        }
        Some(CheckRefusal::Regenerate { change, paths }) => {
            regeneration_status(*change, paths.clone())
        }
        Some(CheckRefusal::Oversize { limit, files }) => {
            super::size_limit::status(*limit, files.clone())
        }
        None if !markers.is_empty() => LandStatus::Refused {
            reason: "conflict_markers".into(),
            message: land_body(LandError::ConflictMarkers(markers.to_vec())).message,
            paths: markers.to_vec(),
        },
        None => return Ok(None),
    };
    let LandStatus::Refused {
        reason,
        message,
        paths,
    } = status
    else {
        return Err(internal(
            "a land check built a refusal that is not one".into(),
        ));
    };
    Ok(Some(LandRefusal {
        reason,
        message,
        paths,
    }))
}

/// The files a refusal names, as the land's answer names them: the stored
/// conflicts, or the paths its merge would store conflicts in.
fn refusal_paths(refusal: Refusal, merge: &MergeCheck, stored: &[StoredConflict]) -> Vec<String> {
    match (refusal, merge) {
        (Refusal::Conflicts, _) => {
            let mut paths: Vec<String> = stored.iter().map(|c| c.path.clone()).collect();
            paths.sort();
            paths.dedup();
            paths
        }
        (Refusal::MergeConflict, MergeCheck::Conflicted { paths, .. }) => paths.clone(),
        _ => Vec::new(),
    }
}

fn merge_gate(merge: MergeCheck, stored: Vec<StoredConflict>, markers: Vec<String>) -> MergeGate {
    match merge {
        MergeCheck::Clean(_) if !markers.is_empty() => {
            MergeGate::ConflictMarkers { paths: markers }
        }
        MergeCheck::Clean(_) => MergeGate::Clean,
        MergeCheck::StoredConflicts => MergeGate::Conflicts { conflicts: stored },
        MergeCheck::Conflicted { change, paths } => MergeGate::MergeConflict { change, paths },
        MergeCheck::Regenerate { change, paths } => {
            MergeGate::RegenerationRequired { change, paths }
        }
        MergeCheck::Oversize { limit, files } => MergeGate::OversizeFiles {
            limit,
            files: files
                .into_iter()
                .map(|(path, bytes)| OversizeFile { path, bytes })
                .collect(),
        },
        MergeCheck::NotChecked => MergeGate::NotChecked,
    }
}

fn receipt_gate(
    engine: &Landing,
    base_ref: &str,
    receipt: ReceiptState,
) -> Result<ReceiptGate, ErrorBody> {
    let landed = |commits: Vec<_>| -> Result<Vec<LandedSince>, ErrorBody> {
        commits
            .into_iter()
            .map(|commit| {
                let change = engine
                    .ledger()
                    .landed_change(base_ref, &commit)
                    .map_err(land_body)?;
                Ok(LandedSince { commit, change })
            })
            .collect()
    };
    Ok(match receipt {
        ReceiptState::Valid(signed) => ReceiptGate::Valid {
            receipt: binding(&signed),
        },
        ReceiptState::Reusable(signed, since) => ReceiptGate::Reusable {
            receipt: binding(&signed),
            landed_since: landed(since)?,
        },
        ReceiptState::Stale(signed, hit) => ReceiptGate::Stale {
            receipt: binding(&signed),
            intersecting: landed(hit)?,
        },
        ReceiptState::Unbound => ReceiptGate::Unbound,
        ReceiptState::Missing => ReceiptGate::Missing,
        ReceiptState::NotRequired => ReceiptGate::NotRequired,
        ReceiptState::NotChecked => ReceiptGate::NotChecked,
    })
}

/// The receipt as its landed trailer would name it (R14): the submitter at
/// the machine whose key signed it.
fn binding(signed: &SignedReceipt) -> ReceiptBinding {
    let receipt = &signed.receipt;
    let submitter = receipt.submitter.on_machine(&signed.machine).map_or_else(
        |_| receipt.submitter.as_str().to_owned(),
        |agent| agent.as_str().to_owned(),
    );
    ReceiptBinding {
        check_set: receipt.check_set.clone(),
        head: receipt.head,
        base: receipt.base,
        submitter,
    }
}
