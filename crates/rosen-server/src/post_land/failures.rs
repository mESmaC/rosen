//! Confirmed failures survive restart whether or not R12 calls for an inverse.
use crate::{events::Staged, ServerState};
use rosen_core::{ChangeId, ReviewMode};
use rosen_protocol::{
    BisectCulprit, CheckKind, ErrorBody, ErrorCode, EventKind, Outcome, PostLandFailure, Response,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::sync::Arc;

type Result<T> = std::result::Result<T, ErrorBody>;
fn internal(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, error.to_string())
}

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS post_land_failures (
        session INTEGER PRIMARY KEY, change_id TEXT NOT NULL, commit_id TEXT NOT NULL,
        kind TEXT NOT NULL CHECK(kind IN ('build','test')), output TEXT NOT NULL, requires_revert INTEGER NOT NULL, revert TEXT);",
    )?;
    unique_culprits(conn)?;
    super::submitter::add_columns(conn, "post_land_failures")?;
    // The status read lists refusals on a read-only connection.
    crate::landing::revert_refused::ensure_schema(conn)
}

fn unique_culprits(conn: &Connection) -> rusqlite::Result<()> {
    let has_current: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
         WHERE type='index' AND name='post_land_failure_culprit_platform')",
        [],
        |r| r.get(0),
    )?;
    let has_legacy: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
         WHERE type='index' AND name='post_land_failure_culprit')",
        [],
        |r| r.get(0),
    )?;
    if has_current && !has_legacy {
        return Ok(());
    }
    if conn.is_autocommit() {
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        unique_culprits(&tx)?;
        return tx.commit();
    }
    super::submitter::add_column(
        conn,
        "post_land_failures",
        "platform",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    // Keep the first report per (commit, kind, platform), including an inverse
    // completed by a later session. Completed bisections retain batch coverage
    // independently of this row.
    conn.execute_batch(
        "UPDATE post_land_failures AS f SET revert=(
            SELECT MAX(other.revert) FROM post_land_failures other
            WHERE other.commit_id=f.commit_id AND other.kind=f.kind AND other.platform=f.platform)
         WHERE revert IS NULL;
         DELETE FROM post_land_failures WHERE session NOT IN (
            SELECT MIN(session) FROM post_land_failures GROUP BY commit_id, kind, platform);
         DROP INDEX IF EXISTS post_land_failure_culprit;
         CREATE UNIQUE INDEX IF NOT EXISTS post_land_failure_culprit_platform
             ON post_land_failures(commit_id, kind, platform);",
    )
}

/// A known, non-reverting test culprit excludes earlier candidates without
/// claiming they passed. Confirmation still checks the new culprit's parent.
pub fn test_boundary(
    conn: &Connection,
    name: &str,
    green: rosen_core::ObjectId,
    failed: rosen_core::ObjectId,
    platform: Option<&str>,
) -> rusqlite::Result<rosen_core::ObjectId> {
    ensure_schema(conn)?;
    let platform = platform.unwrap_or("");
    let id: String = conn.query_row(
        "SELECT h.commit_id FROM base_history h WHERE h.ref_name=?1
         AND (h.commit_id=?2 OR (
             h.seq < (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?3)
             AND EXISTS (SELECT 1 FROM post_land_failures f WHERE f.commit_id=h.commit_id
                 AND f.kind='test' AND f.platform=?4 AND f.requires_revert=0)))
         ORDER BY h.seq DESC LIMIT 1",
        params![name, green.to_string(), failed.to_string(), platform],
        |r| r.get(0),
    )?;
    id.parse().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

pub fn apply(
    state: &ServerState,
    session: i64,
    kind: CheckKind,
    platform: Option<&str>,
    culprit: &BisectCulprit,
) -> Result<()> {
    // Post-land policy takes effect at the landed commit, like its check commands.
    let policy = state
        .project_config
        .at_commit(&culprit.commit)
        .map_err(internal)?
        .config
        .review;
    let must_revert = failure_reverts(policy, kind);
    let mut conn = crate::metadata::open(&state.config.metadata_path()).map_err(internal)?;
    ensure_schema(&conn).map_err(internal)?;
    crate::bisect::ensure_schema(&conn).map_err(internal)?;
    record(&mut conn, session, kind, platform, culprit, must_revert)?;
    state.events.deliver_staged();
    if must_revert {
        if let Err(error) = revert_recorded(state, &conn, culprit.commit, &culprit.output) {
            state.landing.retry_post_land();
            // A conflict leaves the confirmed failure queryable and the checker
            // free to search later batches. Recovery owns the pending inverse.
            if !matches!(error, crate::landing::LandError::RevertConflict { .. }) {
                return Err(internal(error));
            }
        }
    }
    Ok(())
}

fn record(
    conn: &mut Connection,
    session: i64,
    kind: CheckKind,
    platform: Option<&str>,
    culprit: &BisectCulprit,
    must_revert: bool,
) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    let (submitter, at) = confirmed_by(&tx, session)?;
    let platform_key = platform.unwrap_or("");
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO post_land_failures
        (session,change_id,commit_id,kind,platform,output,requires_revert,submitter,submitted_at)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                session,
                culprit.change,
                culprit.commit.to_string(),
                kind.as_str(),
                platform_key,
                culprit.output,
                must_revert,
                submitter,
                at
            ],
        )
        .map_err(internal)?;
    if inserted != 0 {
        let change: ChangeId = culprit.change.parse().map_err(internal)?;
        let detail = event_detail_for(
            session,
            culprit,
            kind,
            platform,
            must_revert,
            submitter.as_deref(),
            at,
        );
        crate::events::stage(
            &tx,
            &Staged::about(EventKind::PostLandFailure, change, detail),
        )
        .map_err(internal)?;
    }
    tx.commit().map_err(internal)
}

/// Event detail published when a confirmed post-land failure is recorded (R9, R12).
#[cfg(test)]
pub(crate) fn event_detail(
    session: i64,
    culprit: &BisectCulprit,
    kind: CheckKind,
    requires_revert: bool,
    submitter: Option<&str>,
    at: Option<i64>,
) -> serde_json::Value {
    event_detail_for(session, culprit, kind, None, requires_revert, submitter, at)
}

pub(crate) fn event_detail_for(
    session: i64,
    culprit: &BisectCulprit,
    kind: CheckKind,
    platform: Option<&str>,
    requires_revert: bool,
    submitter: Option<&str>,
    at: Option<i64>,
) -> serde_json::Value {
    let mut detail = serde_json::json!({
        "session": session,
        "culprit": culprit.commit,
        "kind": kind,
        "output": culprit.output,
        "requires_revert": requires_revert,
        "submitter": submitter,
        "at": at,
    });
    if let Some(platform) = platform {
        detail["platform"] = serde_json::json!(platform);
    }
    detail
}

/// Who submitted the result that confirmed `session`'s culprit: its last
/// accepted step. None for sessions confirmed before steps were recorded.
fn confirmed_by(conn: &Connection, session: i64) -> Result<(Option<String>, Option<i64>)> {
    let row = conn
        .query_row(
            "SELECT submitter,submitted_at FROM bisection_steps
             WHERE session=?1 ORDER BY step DESC LIMIT 1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(internal)?;
    Ok(row.unwrap_or((None, None)))
}

pub async fn handle(state: &Arc<ServerState>, change: Option<String>) -> Outcome {
    let state = Arc::clone(state);
    let result = tokio::task::spawn_blocking(move || {
        let conn = crate::metadata::open(&state.config.metadata_path()).map_err(internal)?;
        ensure_schema(&conn).map_err(internal)?;
        let change = change
            .map(|id| crate::changes::resolve(&conn, &id).map(|id| id.to_string()))
            .transpose()
            .map_err(internal)?;
        list(&conn, change.as_deref())
    })
    .await;
    match result {
        Ok(Ok(failures)) => Outcome::Ok(Response::PostLandFailures(failures)),
        Ok(Err(error)) => Outcome::Error(error),
        Err(error) => Outcome::Error(internal(error)),
    }
}

fn list(conn: &Connection, change: Option<&str>) -> Result<Vec<PostLandFailure>> {
    let mut query = conn
        .prepare(
            "SELECT session,change_id,commit_id,kind,platform,output,revert,requires_revert,
        submitter,submitted_at
        FROM post_land_failures WHERE (?1 IS NULL OR change_id=?1) ORDER BY session",
        )
        .map_err(internal)?;
    let rows = query
        .query_map([change], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, bool>(7)?,
                (r.get(8)?, r.get(9)?),
            ))
        })
        .map_err(internal)?;
    rows.map(|row| {
        let (session, change, commit, kind, platform, output, revert, requires_revert, by) =
            row.map_err(internal)?;
        let (submitter, submitted_at) = by;
        Ok(PostLandFailure {
            session,
            change,
            commit: commit.parse().map_err(internal)?,
            kind: if kind == "build" {
                CheckKind::Build
            } else {
                CheckKind::Test
            },
            platform: (!platform.is_empty()).then_some(platform),
            output,
            requires_revert,
            revert: revert.map(|id| id.parse().map_err(internal)).transpose()?,
            submitter,
            submitted_at,
        })
    })
    .collect()
}

/// R12 disposition, also checked against the landing model fixture.
pub fn failure_reverts(review: ReviewMode, kind: CheckKind) -> bool {
    review == ReviewMode::BeforeLand || kind == CheckKind::Build
}

fn revert_recorded(
    state: &ServerState,
    conn: &Connection,
    culprit: rosen_core::ObjectId,
    output: &str,
) -> crate::landing::LandResult<()> {
    let commit = crate::landing::revert::revert(state, culprit, output)?;
    conn.execute(
        "UPDATE post_land_failures SET revert=?2 WHERE commit_id=?1 AND requires_revert=1",
        params![culprit.to_hex(), commit.to_hex()],
    )?;
    Ok(())
}

/// Each pending inverse is independent: one refusal must not prevent the rest.
/// False requests backoff; every new base also wakes the recovery worker.
pub(crate) fn retry(state: &ServerState) -> Result<bool> {
    let conn = crate::metadata::open(&state.config.metadata_path()).map_err(internal)?;
    ensure_schema(&conn).map_err(internal)?;
    let mut query = conn
        .prepare(
            "SELECT commit_id,output FROM post_land_failures
        WHERE requires_revert=1 AND revert IS NULL ORDER BY session",
        )
        .map_err(internal)?;
    let pending = query
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;
    let mut complete = true;
    for (culprit, output) in pending {
        let culprit = culprit.parse().map_err(internal)?;
        if let Err(error) = revert_recorded(state, &conn, culprit, &output) {
            eprintln!("rosen-server: revert {culprit} pending: {error}");
            complete = false;
        }
    }
    Ok(complete)
}
