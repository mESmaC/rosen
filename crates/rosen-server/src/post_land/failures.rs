//! Confirmed failures survive restart whether or not R12 calls for an inverse.
use crate::{events::Staged, ServerState};
use rosen_core::{ChangeId, ReviewMode};
use rosen_protocol::{
    BisectCulprit, CheckKind, ErrorBody, ErrorCode, EventKind, Outcome, PostLandFailure, Response,
};
use rusqlite::{params, Connection, TransactionBehavior};
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
    )
}

pub fn apply(
    state: &ServerState,
    session: i64,
    kind: CheckKind,
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
    record(&mut conn, session, kind, culprit, must_revert)?;
    state.events.deliver_staged();
    if must_revert {
        let commit = crate::landing::revert::revert(state, culprit.commit, &culprit.output)
            .map_err(internal)?;
        conn.execute(
            "UPDATE post_land_failures SET revert=?2 WHERE session=?1",
            params![session, commit.to_string()],
        )
        .map_err(internal)?;
    }
    Ok(())
}

fn record(
    conn: &mut Connection,
    session: i64,
    kind: CheckKind,
    culprit: &BisectCulprit,
    must_revert: bool,
) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO post_land_failures
        (session,change_id,commit_id,kind,output,requires_revert) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                session,
                culprit.change,
                culprit.commit.to_string(),
                kind.as_str(),
                culprit.output,
                must_revert
            ],
        )
        .map_err(internal)?;
    if inserted != 0 {
        let change: ChangeId = culprit.change.parse().map_err(internal)?;
        let detail = serde_json::json!({"session":session,"culprit":culprit.commit,
            "kind":kind,"output":culprit.output,"requires_revert":must_revert});
        crate::events::stage(
            &tx,
            &Staged::about(EventKind::PostLandFailure, change, detail),
        )
        .map_err(internal)?;
    }
    tx.commit().map_err(internal)
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
            "SELECT session,change_id,commit_id,kind,output,revert,requires_revert
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
                r.get(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, bool>(6)?,
            ))
        })
        .map_err(internal)?;
    rows.map(|row| {
        let (session, change, commit, kind, output, revert, requires_revert) =
            row.map_err(internal)?;
        Ok(PostLandFailure {
            session,
            change,
            commit: commit.parse().map_err(internal)?,
            kind: if kind == "build" {
                CheckKind::Build
            } else {
                CheckKind::Test
            },
            output,
            requires_revert,
            revert: revert.map(|id| id.parse().map_err(internal)).transpose()?,
        })
    })
    .collect()
}

/// R12 disposition, also checked against the landing model fixture.
pub fn failure_reverts(review: ReviewMode, kind: CheckKind) -> bool {
    review == ReviewMode::BeforeLand || kind == CheckKind::Build
}
