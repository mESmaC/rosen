//! Restart-safe post-land bisection. Checks execute outside Rosen (D9, R12).
pub mod failure;
mod known;
mod model;
mod progress;
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
    let (green, failed) = range;
    storage::ensure_schema(conn).map_err(internal)?;
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM bisections WHERE ref_name=?1 AND failed=?2 AND kind=?3",
            params![name, failed.to_string(), kind.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?
    {
        return storage::load(conn, id)?.state();
    }
    let (boundary, known_output) = known::context(conn, name, green, failed, kind)?;
    let commits = interval(conn, name, boundary, failed)?;
    let high = commits.len() - 1;
    let phase = if known_output.as_deref() == Some(output) {
        "known"
    } else if high == 1 {
        "confirm"
    } else {
        "search"
    };
    conn.execute(
        "INSERT INTO bisections
        (ref_name,failed,commits,low,high,phase,step,output,kind,known_output) VALUES (?1,?2,?3,0,?4,?5,0,?6,?7,?8)",
        params![
            name,
            failed.to_string(),
            serde_json::to_string(&commits).map_err(internal)?,
            high,
            phase,
            output,
            kind.as_str(),
            known_output
        ],
    )
    .map_err(internal)?;
    let session = storage::load(conn, conn.last_insert_rowid())?;
    progress::opened(conn, &session, failed, opened_by)?;
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
    let planned = progress::planned_steps(session.commits.len() - 1);
    session.advance(result.passed, &result.output);
    storage::save(&tx, &session)?;
    let state = session.state()?;
    progress::accepted(&tx, &result.job, result.passed, by, &state, planned)?;
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
            Some((result, by)) => submit_as(&mut conn, &result, Some(&by))?,
            None => next(&conn, id)?,
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

fn apply_confirmed(state: &crate::ServerState, answer: &BisectState) -> Result<()> {
    if let Some(culprit) = &answer.culprit {
        crate::post_land::failures::apply(state, answer.id, answer.kind, culprit)?;
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
