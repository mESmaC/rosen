//! `post_land_status`: R12's progress as stored, read without changing it.
//! It runs on the registry's read-only WAL connections, so it never offers
//! a job, moves a session or waits behind a land's write.
use crate::ServerState;
use rosen_core::ObjectId;
use rosen_protocol::{
    CheckKind, ErrorBody, ErrorCode, Outcome, PendingInverse, PostLandStatus, Response,
};
use rusqlite::{Connection, OptionalExtension};
use std::sync::Arc;

type Result<T> = std::result::Result<T, ErrorBody>;

fn internal(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, error.to_string())
}

pub async fn handle(state: &Arc<ServerState>) -> Outcome {
    let state = Arc::clone(state);
    let work = tokio::task::spawn_blocking(move || {
        let mut answer = None;
        state
            .changes
            .with_read(|conn| {
                answer = Some(read(conn));
                Ok(())
            })
            .map_err(internal)?;
        answer.unwrap_or_else(|| Err(internal("status read did not run")))
    });
    match work.await {
        Ok(Ok(status)) => Outcome::Ok(Response::PostLandStatus(status)),
        Ok(Err(error)) => Outcome::Error(error),
        Err(error) => Outcome::Error(internal(error)),
    }
}

/// Startup installs every table read here. A server that has never run
/// post-land work may still lack some, which reads as nothing recorded.
fn read(conn: &Connection) -> Result<PostLandStatus> {
    let Some((name, base)) = project_base(conn)? else {
        return Ok(PostLandStatus {
            base: None,
            last_built: None,
            last_green: None,
            sessions: Vec::new(),
            pending_inverses: Vec::new(),
        });
    };
    let has = |table: &str| table_exists(conn, table);
    let last_green = if has("post_land_green")? {
        super::store::checkpoint(conn, &name).map_err(internal)?
    } else {
        None
    };
    let last_built = if has("post_land_build_green")? {
        super::builds::checkpoint(conn, &name).map_err(internal)?
    } else {
        None
    };
    let checkpoints = has("post_land_green")? && has("post_land_build_green")?;
    let sessions = if checkpoints && has("bisection_steps")? {
        crate::bisect::sessions(conn, &name)?
    } else {
        Vec::new()
    };
    let pending_inverses = if has("post_land_failures")? && has("revert_refusals")? {
        pending_inverses(conn, &name)?
    } else {
        Vec::new()
    };
    Ok(PostLandStatus {
        base: Some(base),
        last_built,
        last_green,
        sessions,
        pending_inverses,
    })
}

/// The project's base ref and tip, as `Ledger::project_base` reads them,
/// but from the same snapshot as the rest of the status.
fn project_base(conn: &Connection) -> Result<Option<(String, ObjectId)>> {
    if !table_exists(conn, "base_refs")? {
        return Ok(None);
    }
    let row: Option<(String, String)> = conn
        .query_row("SELECT name, target FROM base_refs LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()
        .map_err(internal)?;
    row.map(|(name, tip)| Ok((name, tip.parse().map_err(internal)?)))
        .transpose()
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )
    .map_err(internal)
}

/// Confirmed culprits whose required inverse has neither landed nor been
/// completed by another session, with their latest refusal, if any.
fn pending_inverses(conn: &Connection, name: &str) -> Result<Vec<PendingInverse>> {
    let reverted = if table_exists(conn, "auto_reverts")? {
        "AND NOT EXISTS(SELECT 1 FROM auto_reverts a WHERE a.culprit=f.commit_id)"
    } else {
        ""
    };
    let mut query = conn
        .prepare(&format!(
            "SELECT f.commit_id,f.change_id,f.kind,r.base,r.paths,r.overlapping,r.reason,
             (SELECT MIN(refused_at) FROM revert_refusals WHERE culprit=f.commit_id)
             FROM post_land_failures f
             JOIN base_history h ON h.ref_name=?1 AND h.commit_id=f.commit_id
             LEFT JOIN revert_refusals r ON r.rowid=(SELECT rowid FROM revert_refusals
                 WHERE culprit=f.commit_id ORDER BY COALESCE(refused_at,0) DESC, rowid DESC LIMIT 1)
             WHERE f.requires_revert=1 AND f.revert IS NULL {reverted}
             ORDER BY h.seq"
        ))
        .map_err(internal)?;
    let rows = query
        .query_map([name], |r| {
            Ok(Row {
                culprit: r.get(0)?,
                change: r.get(1)?,
                kind: r.get(2)?,
                base: r.get(3)?,
                paths: r.get(4)?,
                overlapping: r.get(5)?,
                reason: r.get(6)?,
                first_refused_at: r.get(7)?,
            })
        })
        .map_err(internal)?;
    rows.map(|row| row.map_err(internal)?.inverse()).collect()
}

struct Row {
    culprit: String,
    change: String,
    kind: String,
    base: Option<String>,
    paths: Option<String>,
    overlapping: Option<String>,
    reason: Option<String>,
    first_refused_at: Option<i64>,
}

impl Row {
    fn inverse(self) -> Result<PendingInverse> {
        Ok(PendingInverse {
            culprit: self.culprit.parse().map_err(internal)?,
            change: self.change,
            kind: if self.kind == "build" {
                CheckKind::Build
            } else {
                CheckKind::Test
            },
            attempted_base: self
                .base
                .map(|base| base.parse().map_err(internal))
                .transpose()?,
            // Refusals recorded before these columns read as empty lists.
            paths: parse_list(self.paths.as_deref())?,
            overlapping_commits: parse_list(self.overlapping.as_deref())?
                .iter()
                .map(|commit| commit.parse().map_err(internal))
                .collect::<Result<_>>()?,
            reason: self.reason,
            first_refused_at: self.first_refused_at,
        })
    }
}

/// A stored JSON list of strings: paths, or commit IDs in hex.
fn parse_list(text: Option<&str>) -> Result<Vec<String>> {
    text.map(|text| serde_json::from_str(text).map_err(internal))
        .transpose()
        .map(Option::unwrap_or_default)
}
