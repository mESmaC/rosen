//! Bind the batch failure to stored history and configuration before bisection.
use crate::{landing::Ledger, post_land, ServerState};
use rosen_core::ObjectId;
use rosen_protocol::{BisectState, ErrorBody, ErrorCode, Outcome, PostLandJob, Response};
use rusqlite::TransactionBehavior;
use std::sync::Arc;

pub async fn handle(state: &Arc<ServerState>, job: PostLandJob, output: String) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || start(&state, &job, &output)).await {
        Ok(Ok(state)) => Outcome::Ok(Response::Bisection(state)),
        Ok(Err(error)) => Outcome::Error(error),
        Err(error) => Outcome::Error(super::internal(error)),
    }
}

pub fn start(
    state: &ServerState,
    job: &PostLandJob,
    output: &str,
) -> Result<BisectState, ErrorBody> {
    let ledger = Ledger::open(&state.config.metadata_path()).map_err(super::internal)?;
    let (name, _) = ledger
        .project_base()
        .map_err(super::internal)?
        .ok_or_else(|| ErrorBody::new(ErrorCode::Refused, "project has no base"))?;
    post_land::validate(state, &ledger, &name, job)?;
    let mut conn = crate::metadata::open(&state.config.metadata_path()).map_err(super::internal)?;
    post_land::store::ensure_schema(&conn).map_err(super::internal)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(super::internal)?;
    let green = match post_land::store::last_green(&tx, &name).map_err(super::internal)? {
        Some(green) => green,
        None => {
            // No initial commit is assumed tested: confirmation will run the
            // culprit's parent, and refuses to identify a culprit if it fails.
            let initial: String = tx
                .query_row(
                    "SELECT commit_id FROM base_history
                WHERE ref_name=?1 ORDER BY seq LIMIT 1",
                    [&name],
                    |r| r.get(0),
                )
                .map_err(super::internal)?;
            ObjectId::from_hex(&initial).map_err(super::internal)?
        }
    };
    let answer = super::begin_in(&tx, &name, green, job.commit, output)?;
    tx.commit().map_err(super::internal)?;
    Ok(answer)
}
