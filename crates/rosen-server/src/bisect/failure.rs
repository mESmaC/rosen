//! Bind the batch failure to stored history and configuration before bisection.
use crate::post_land::{self, submitter::Submitted};
use crate::{landing::Ledger, ServerState};
use rosen_core::ObjectId;
use rosen_protocol::{BisectState, ErrorBody, ErrorCode, Outcome, PostLandJob, Response};
use rusqlite::TransactionBehavior;
use std::sync::Arc;

pub async fn handle(
    state: &Arc<ServerState>,
    job: PostLandJob,
    output: String,
    by: Submitted,
) -> Outcome {
    let state = Arc::clone(state);
    let work = move || {
        let answer = start(&state, &job, &output, Some(&by));
        if answer.is_ok() {
            state.stats.bump();
        }
        state.events.deliver_staged();
        answer
    };
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(state)) => Outcome::Ok(Response::Bisection(state)),
        Ok(Err(error)) => Outcome::Error(error),
        Err(error) => Outcome::Error(super::internal(error)),
    }
}

pub fn start(
    state: &ServerState,
    job: &PostLandJob,
    output: &str,
    by: Option<&Submitted>,
) -> Result<BisectState, ErrorBody> {
    let ledger = Ledger::open(&state.config.metadata_path()).map_err(super::internal)?;
    let (name, _) = ledger
        .project_base()
        .map_err(super::internal)?
        .ok_or_else(|| ErrorBody::new(ErrorCode::Refused, "project has no base"))?;
    post_land::validate(state, &ledger, &name, job)?;
    let mut conn = crate::metadata::open(&state.config.metadata_path()).map_err(super::internal)?;
    post_land::store::ensure_schema(&conn).map_err(super::internal)?;
    post_land::platforms::ensure_schema(&conn).map_err(super::internal)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(super::internal)?;
    let boundary = search_boundary(&tx, &name, job)?;
    let range = (boundary, job.commit);
    let at_ms = by.map_or_else(crate::stats::now_millis, |s| s.at);
    let offered = state
        .stats
        .take_post_land_offer(job.kind, job.platform.as_deref(), job.commit);
    let (started_at, durations) = crate::stats::resolve_job_timing(job, offered, at_ms);
    let timing =
        crate::stats::store::CheckpointTiming::from_resolved(started_at, at_ms, durations.as_ref());
    let answer = super::begin_for_in_with_timing(
        &tx,
        &name,
        range,
        output,
        job.kind,
        job.platform.as_deref(),
        by,
        Some(timing),
    )?;
    tx.commit().map_err(super::internal)?;
    if let Some(next_job) = &answer.job {
        state
            .stats
            .record_bisect_offer(next_job.session, next_job.step, at_ms);
    }
    Ok(answer)
}

fn search_boundary(
    conn: &rusqlite::Connection,
    name: &str,
    job: &PostLandJob,
) -> Result<ObjectId, ErrorBody> {
    // Pass the verified green/built checkpoint (or initial commit) so `known::context`
    // sees every earlier confirmed culprit in `[green, failed)` and computes the
    // search lower bound uniformly for both build/lint and test checks.
    match post_land::last_green_for(conn, name, job.kind, job.platform.as_deref())? {
        Some(green) => Ok(green),
        None => {
            // No initial commit is assumed tested: confirmation will run the
            // culprit's parent, and refuses to identify a culprit if it fails.
            let initial: String = conn
                .query_row(
                    "SELECT commit_id FROM base_history
                WHERE ref_name=?1 ORDER BY seq LIMIT 1",
                    [name],
                    |r| r.get(0),
                )
                .map_err(super::internal)?;
            ObjectId::from_hex(&initial).map_err(super::internal)
        }
    }
}
