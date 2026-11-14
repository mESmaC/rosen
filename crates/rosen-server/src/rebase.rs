//! Explicit server-side rebases (SPEC R2). No local path is accepted here.
use crate::history::{self, HistoryError, HistoryResult};
use crate::{changes, ServerState};
use rosen_core::{Conflict, ObjectId};
use rosen_protocol::{ChangeRecord, ErrorBody, EventKind, Outcome, Response};
use std::sync::Arc;

pub async fn handle(state: &Arc<ServerState>, change: String) -> Outcome {
    let permit = match Arc::clone(&state.workspace_io).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return Outcome::Error(ErrorBody::new(
                rosen_protocol::ErrorCode::ShuttingDown,
                "workspace I/O closed",
            ))
        }
    };
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        rebase(&state, &change)
    })
    .await
    {
        Ok(Ok(record)) => Outcome::Ok(Response::Change(record)),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(err.code(), err.to_string())),
        Err(err) => Outcome::Error(ErrorBody::new(
            rosen_protocol::ErrorCode::Internal,
            err.to_string(),
        )),
    }
}

/// Rebase an unstacked change onto the base. Stacked changes retain their
/// dependency until the landing restacker moves them (R8 step 4).
///
/// A rebase that conflicts still moves the change onto the base (R5): its
/// new head holds diff3 markers in each conflicting file, and those files'
/// conflicts are stored open with it. The owner applies the head (R2),
/// combines both sides, and `rosen resolve` clears the conflicts; the next
/// land then measures the owner's edits from the new base, so keeping both
/// sides of a land conflict lands. Stored conflicts never block a rebase,
/// and a rebase never clears one: only the owner's resolve does.
pub fn rebase(state: &ServerState, change: &str) -> HistoryResult<ChangeRecord> {
    let before = state.changes.show(change)?;
    if matches!(before.change.parent, rosen_core::ChangeParent::Change(_)) {
        return Err(HistoryError::Refused(
            "land the parent change before rebasing onto base".into(),
        ));
    }
    let ctx = history::open(state)?;
    let merged = history::merge::merge_result(&ctx, change)?;
    if merged.head != before.change.head {
        return Err(HistoryError::Refused("head moved; retry the rebase".into()));
    }
    let (tree, conflicts, regenerate) = match merged.tree {
        Some(tree) => (tree, Vec::new(), merged.regenerate),
        None => marked(&ctx, &before.change, &merged.base)?,
    };
    let paths: Vec<String> = conflicts.iter().map(|(path, _)| path.clone()).collect();
    let record = state.changes.with_conn(|conn| {
        changes::rebase_checked_storing(conn, &before.change, merged.base, tree, conflicts)
    })?;
    let id = record.change.id.to_string();
    state.events.publish(
        EventKind::Rebased,
        Some(id.clone()),
        serde_json::json!({
            "onto": merged.base,
            "tree": tree,
            "regenerate": regenerate,
            "conflicts": paths,
        }),
    );
    if !paths.is_empty() {
        state.events.publish(
            EventKind::Conflict,
            Some(id),
            serde_json::json!({ "paths": paths }),
        );
    }
    crate::stacks::refresh(state);
    Ok(record)
}

type Regions = Vec<(String, Vec<Conflict>)>;

/// The marked head of a conflicting rebase, each conflicting file's regions
/// with the change's side as `ours` (as a land stores them, R8), and the
/// generated files to regenerate.
fn marked(
    ctx: &history::Ctx,
    change: &rosen_core::Change,
    onto: &ObjectId,
) -> HistoryResult<(ObjectId, Regions, Vec<String>)> {
    let (merge, [merge_base, onto_tree]) = history::merge::merge_marked(ctx, change, onto)?;
    let Some(tree) = merge.tree else {
        return Err(HistoryError::Refused(format!(
            "rebase conflicts in {}: a file and a directory collide",
            merge.conflicts.join(", ")
        )));
    };
    let trees = [merge_base, change.head, onto_tree];
    let regions = merge
        .conflicts
        .iter()
        .map(|path| {
            crate::landing::conflict_routes::regions(&ctx.store, trees, path)
                .map_err(|err| HistoryError::Refused(err.to_string()))
        })
        .collect::<HistoryResult<Regions>>()?;
    Ok((tree, regions, merge.regenerate))
}
