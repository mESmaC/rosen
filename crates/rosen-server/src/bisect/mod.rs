//! Restart-safe post-land bisection. Checks execute outside Rosen (D9, R12).
mod model;
mod storage;

pub use model::midpoint;
use rosen_core::ObjectId;
use rosen_protocol::{BisectCulprit, BisectResult, BisectState, ErrorBody, ErrorCode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

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
    storage::ensure_schema(conn).map_err(internal)?;
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM bisections WHERE ref_name=?1 AND failed=?2",
            params![name, failed.to_string()],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?
    {
        return storage::load(conn, id)?.state();
    }
    let commits = interval(conn, name, green, failed)?;
    let high = commits.len() - 1;
    let phase = if high == 1 { "confirm" } else { "search" };
    conn.execute(
        "INSERT INTO bisections
        (ref_name,failed,commits,low,high,phase,step,output) VALUES (?1,?2,?3,0,?4,?5,0,?6)",
        params![
            name,
            failed.to_string(),
            serde_json::to_string(&commits).map_err(internal)?,
            high,
            phase,
            output
        ],
    )
    .map_err(internal)?;
    storage::load(conn, conn.last_insert_rowid())?.state()
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
        WHERE ref_name=?1 AND seq>=?2 AND seq<=?3 ORDER BY seq",
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

/// Reject duplicate/stale results, including concurrent replies to one job.
pub fn submit(conn: &mut Connection, result: &BisectResult) -> Result<BisectState> {
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
    session.advance(result.passed, &result.output);
    storage::save(&tx, &session)?;
    let state = session.state()?;
    tx.commit().map_err(internal)?;
    Ok(state)
}

pub fn confirmed(conn: &Connection, id: i64) -> Result<Option<BisectCulprit>> {
    Ok(next(conn, id)?.culprit)
}

pub async fn handle(
    state: &std::sync::Arc<crate::ServerState>,
    id: i64,
    result: Option<BisectResult>,
) -> rosen_protocol::Outcome {
    let path = state.config.metadata_path();
    let work = tokio::task::spawn_blocking(move || {
        let mut conn = crate::metadata::open(&path).map_err(internal)?;
        match result {
            Some(result) => submit(&mut conn, &result),
            None => next(&conn, id),
        }
    })
    .await;
    match work {
        Ok(Ok(state)) => rosen_protocol::Outcome::Ok(rosen_protocol::Response::Bisection(state)),
        Ok(Err(error)) => rosen_protocol::Outcome::Error(error),
        Err(error) => rosen_protocol::Outcome::Error(internal(error)),
    }
}
