//! Restart-safe post-land bisection. Checks execute outside Rosen (D9, R12).
pub mod failure;
mod known;
mod model;
pub(crate) mod progress;
mod storage;

use crate::post_land::submitter::Submitted;
pub use model::midpoint;
pub use progress::{planned_steps, sessions};
use rosen_core::ObjectId;
use rosen_protocol::{BisectCulprit, BisectResult, BisectState, ErrorBody, ErrorCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
pub(crate) use storage::ensure_schema;

type Result<T> = std::result::Result<T, ErrorBody>;

pub(super) fn internal(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, error.to_string())
}

/// Called in the batch failure transaction, with its captured last-green.
/// The imported base is the lower boundary when no landed batch passed yet.
pub fn begin(
    conn: &mut Connection,
    name: &str,
    green: ObjectId,
    failed: ObjectId,
    output: &str,
) -> Result<BisectState> {
    storage::ensure_schema(conn).map_err(internal)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    let state = begin_in(&tx, name, green, failed, output)?;
    tx.commit().map_err(internal)?;
    Ok(state)
}

/// Variant for the batch result's existing write transaction.
pub fn begin_in(
    conn: &Connection,
    name: &str,
    green: ObjectId,
    failed: ObjectId,
    output: &str,
) -> Result<BisectState> {
    let kind = rosen_protocol::CheckKind::Test;
    begin_kind_in(conn, name, (green, failed), output, kind, None)
}

/// Opens or returns the session for the failing batch `range.1` above
/// `range.0`. A new session records `opened_by` and stages its event.
pub fn begin_kind_in(
    conn: &Connection,
    name: &str,
    range: (ObjectId, ObjectId),
    output: &str,
    kind: rosen_protocol::CheckKind,
    opened_by: Option<&Submitted>,
) -> Result<BisectState> {
    begin_for_in(conn, name, range, output, kind, None, opened_by)
}

/// Per-platform variant of [`begin_kind_in`]: each required platform runs an
/// independent bisection session for a failing batch (R12, #440).
pub fn begin_for_in(
    conn: &Connection,
    name: &str,
    range: (ObjectId, ObjectId),
    output: &str,
    kind: rosen_protocol::CheckKind,
    platform: Option<&str>,
    opened_by: Option<&Submitted>,
) -> Result<BisectState> {
    begin_for_in_with_timing(conn, name, range, output, kind, platform, opened_by, None)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn begin_for_in_with_timing(
    conn: &Connection,
    name: &str,
    range: (ObjectId, ObjectId),
    output: &str,
    kind: rosen_protocol::CheckKind,
    platform: Option<&str>,
    opened_by: Option<&Submitted>,
    timing: Option<crate::stats::store::CheckpointTiming<'_>>,
) -> Result<BisectState> {
    let (green, failed) = range;
    let platform_key = platform.unwrap_or("");
    storage::ensure_schema(conn).map_err(internal)?;
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM bisections WHERE ref_name=?1 AND failed=?2 AND kind=?3 AND platform=?4",
            params![name, failed.to_string(), kind.as_str(), platform_key],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?
    {
        return storage::load(conn, id)?.state();
    }
    let (boundary, known_ctx) = known::context(conn, name, green, failed, kind, platform)?;
    let commits = interval(conn, name, boundary, failed)?;
    let high = commits.len() - 1;
    let known = known_ctx.bind(&commits);
    let phase = if known.matches_at(high, output) {
        "known"
    } else if high == 1 {
        "confirm"
    } else {
        "search"
    };
    let known_stored = known.to_stored()?;
    let at_ms = opened_by.map_or_else(crate::stats::now_millis, |s| s.at);
    conn.execute(
        "INSERT INTO bisections
        (ref_name,failed,commits,low,high,phase,step,output,kind,known_output,step_offered_at,platform)
        VALUES (?1,?2,?3,0,?4,?5,0,?6,?7,?8,?9,?10)",
        params![
            name,
            failed.to_string(),
            serde_json::to_string(&commits).map_err(internal)?,
            high,
            phase,
            output,
            kind.as_str(),
            known_stored,
            at_ms,
            platform_key
        ],
    )
    .map_err(internal)?;
    let session = storage::load(conn, conn.last_insert_rowid())?;
    progress::opened(conn, &session, failed, opened_by)?;
    let outcome = if phase == "known" {
        "known"
    } else {
        "bisecting"
    };
    let checkpoint_timing =
        timing.unwrap_or_else(|| crate::stats::store::CheckpointTiming::at(at_ms));
    crate::stats::store::record_checkpoint_bisect_opened(
        conn,
        name,
        kind,
        platform,
        &failed,
        high as u64,
        session.id,
        outcome,
        checkpoint_timing,
    )
    .map_err(internal)?;
    session.state()
}

fn interval(
    conn: &Connection,
    name: &str,
    green: ObjectId,
    failed: ObjectId,
) -> Result<Vec<(ObjectId, Option<String>)>> {
    let position = |id: ObjectId| {
        conn.query_row(
            "SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2",
            params![name, id.to_string()],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::InvalidArgument,
                "boundary is not in landed history",
            )
        })
    };
    let (low, high) = (position(green)?, position(failed)?);
    if low >= high {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "empty or reversed failing range",
        ));
    }
    let mut query = conn
        .prepare(
            "SELECT commit_id,change_id FROM base_history
        WHERE ref_name=?1 AND seq>=?2 AND seq<=?3 AND tier != 'mirror' ORDER BY seq",
        )
        .map_err(internal)?;
    let rows = query
        .query_map(params![name, low, high], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(internal)?;
    rows.map(|row| {
        let (commit, change) = row.map_err(internal)?;
        Ok((ObjectId::from_hex(&commit).map_err(internal)?, change))
    })
    .collect()
}

pub fn next(conn: &Connection, id: i64) -> Result<BisectState> {
    storage::ensure_schema(conn).map_err(internal)?;
    storage::load(conn, id)?.state()
}

pub fn submit(conn: &mut Connection, result: &BisectResult) -> Result<BisectState> {
    submit_as(conn, result, None)
}

/// Reject duplicate/stale results, including concurrent replies to one job.
/// An accepted result is recorded with `by` and stages one `bisect` event.
pub fn submit_as(
    conn: &mut Connection,
    result: &BisectResult,
    by: Option<&Submitted>,
) -> Result<BisectState> {
    submit_as_with_offer(conn, result, by, None)
}

pub(crate) fn submit_as_with_offer(
    conn: &mut Connection,
    result: &BisectResult,
    by: Option<&Submitted>,
    offered_fallback: Option<i64>,
) -> Result<BisectState> {
    storage::ensure_schema(conn).map_err(internal)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    let mut session = storage::load(&tx, result.job.session)?;
    if session.state()?.job.as_ref() != Some(&result.job) {
        return Err(ErrorBody::new(
            ErrorCode::Refused,
            "stale or mismatched bisection job",
        ));
    }
    let at_ms = by.map_or_else(crate::stats::now_millis, |s| s.at);
    let step_durations = crate::stats::resolve_bisect_step_durations(
        session.kind,
        result.durations.as_ref(),
        offered_fallback.or(session.step_offered_at),
        at_ms,
    );
    let planned = progress::planned_steps(session.commits.len() - 1);
    session.advance(result.passed, &result.output);
    session.step_offered_at = Some(at_ms);
    storage::save(&tx, &session)?;
    let state = session.state()?;
    progress::accepted(&tx, &result.job, result.passed, by, &state, planned)?;
    let steps: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM bisection_steps WHERE session = ?1",
            [session.id],
            |r| r.get(0),
        )
        .map_err(internal)?;
    let outcome = if state.culprit.is_some() {
        "culprit"
    } else if state.flaky {
        "flaky"
    } else {
        "bisecting"
    };
    let recorded = step_durations.as_ref().map(|(durations, estimated)| {
        crate::stats::store::RecordedDurations {
            durations,
            estimated: *estimated,
        }
    });
    crate::stats::store::record_checkpoint_bisect_step(
        &tx,
        session.id,
        u64::try_from(steps).unwrap_or(0),
        outcome,
        at_ms,
        recorded,
    )
    .map_err(internal)?;
    tx.commit().map_err(internal)?;
    Ok(state)
}

pub fn confirmed(conn: &Connection, id: i64) -> Result<Option<BisectCulprit>> {
    Ok(next(conn, id)?.culprit)
}

pub async fn handle(
    state: &std::sync::Arc<crate::ServerState>,
    id: i64,
    result: Option<(BisectResult, Submitted)>,
) -> rosen_protocol::Outcome {
    let state = std::sync::Arc::clone(state);
    let path = state.config.metadata_path();
    let work = tokio::task::spawn_blocking(move || {
        let mut conn = crate::metadata::open(&path).map_err(internal)?;
        let answer = match result {
            Some((result, by)) => {
                let offered = state
                    .stats
                    .take_bisect_offer(result.job.session, result.job.step);
                let answer = submit_as_with_offer(&mut conn, &result, Some(&by), offered)?;
                if let Some(next_job) = &answer.job {
                    state
                        .stats
                        .record_bisect_offer(next_job.session, next_job.step, by.at);
                }
                state.stats.bump();
                answer
            }
            None => {
                let answer = next(&conn, id)?;
                if let Some(next_job) = &answer.job {
                    state.stats.record_bisect_offer(
                        next_job.session,
                        next_job.step,
                        crate::stats::now_millis(),
                    );
                }
                answer
            }
        };
        // The step's event committed with it; a stop before this delivers
        // it at the next start (R9).
        state.events.deliver_staged();
        apply_confirmed(&state, &answer)?;
        Ok::<_, ErrorBody>(answer)
    })
    .await;
    match work {
        Ok(Ok(state)) => rosen_protocol::Outcome::Ok(rosen_protocol::Response::Bisection(state)),
        Ok(Err(error)) => rosen_protocol::Outcome::Error(error),
        Err(error) => rosen_protocol::Outcome::Error(internal(error)),
    }
}

/// Failing test names parsed from `output`, excluding tests already explained by
/// earlier known culprits recorded on `session`.
pub(crate) fn failing_tests_for(conn: &Connection, session: i64, output: &str) -> Vec<String> {
    let row: Option<(Option<String>, usize)> = conn
        .query_row(
            "SELECT known_output, high FROM bisections WHERE id=?1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();
    match row {
        Some((known_raw, high)) => {
            known::KnownFailures::from_stored(known_raw).unfamiliar_tests(high, output)
        }
        None => known::parse_failing_tests(output).into_iter().collect(),
    }
}

fn apply_confirmed(state: &crate::ServerState, answer: &BisectState) -> Result<()> {
    if let Some(culprit) = &answer.culprit {
        crate::post_land::failures::apply(
            state,
            answer.id,
            answer.kind,
            answer.platform.as_deref(),
            culprit,
            &answer.failing_tests,
        )?;
    }
    Ok(())
}

/// Confirmation commits before the inverse land. Replay covers that crash gap;
/// the inverse land itself is idempotent by culprit commit. A session whose
/// inverse land fails, as when the revert conflicts or stays contended, is
/// logged and left for the next restart or `bisect next`: it must not keep
/// every later confirmed culprit in the base (issue #177).
pub fn recover(state: &crate::ServerState) -> Result<()> {
    let conn = crate::metadata::open(&state.config.metadata_path()).map_err(internal)?;
    storage::ensure_schema(&conn).map_err(internal)?;
    let mut query = conn
        .prepare("SELECT id FROM bisections WHERE phase='done' ORDER BY id")
        .map_err(internal)?;
    let ids = query
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    let mut failed = 0;
    for id in ids {
        if let Err(err) = next(&conn, id).and_then(|answer| apply_confirmed(state, &answer)) {
            eprintln!("rosen-server: bisection {id}: confirmed culprit not reverted: {err}");
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(internal(format!(
            "{failed} confirmed bisection(s) still wait for their revert"
        )));
    }
    Ok(())
}
