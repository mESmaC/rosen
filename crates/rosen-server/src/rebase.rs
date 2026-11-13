//! Explicit server-side rebases (SPEC R2). No local path is accepted here.
use crate::history::{self, HistoryError, HistoryResult};
use crate::{changes, ServerState};
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
pub fn rebase(state: &ServerState, change: &str) -> HistoryResult<ChangeRecord> {
    let before = state.changes.show(change)?;
    if matches!(before.change.parent, rosen_core::ChangeParent::Change(_)) {
        return Err(HistoryError::Refused(
            "land the parent change before rebasing onto base".into(),
        ));
    }
    if !before.change.conflicts.is_empty() {
        return Err(HistoryError::Refused(
            "resolve stored conflicts before rebasing".into(),
        ));
    }
    let ctx = history::open(state)?;
    let merged = history::merge::merge_result(&ctx, change)?;
    if merged.head != before.change.head {
        return Err(HistoryError::Refused("head moved; retry the rebase".into()));
    }
    let tree = merged.tree.ok_or_else(|| {
        HistoryError::Refused(format!(
            "rebase conflicts in {}",
            merged.conflicts.join(", ")
        ))
    })?;
    let record = state
        .changes
        .with_conn(|conn| changes::rebase_checked(conn, &before.change, merged.base, tree))?;
    state.events.publish(
        EventKind::Rebased,
        Some(record.change.id.to_string()),
        serde_json::json!({"onto": merged.base, "tree": tree, "regenerate": merged.regenerate}),
    );
    Ok(record)
}
