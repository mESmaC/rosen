//! `post_land_status`: R12's progress as stored, read without changing it.
//! It runs on the registry's read-only WAL connections, so it never offers
//! a job, moves a session or waits behind a land's write.
use crate::ServerState;
use rosen_core::{ObjectId, Platform};
use rosen_protocol::{
    CheckKind, ErrorBody, ErrorCode, Outcome, PendingInverse, PlatformStatus, PostLandStatus,
    Response,
};
use rusqlite::{Connection, OptionalExtension};
use std::sync::Arc;

type Result<T> = std::result::Result<T, ErrorBody>;

fn internal(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, error.to_string())
}

pub async fn handle(state: &Arc<ServerState>) -> Outcome {
    match snapshot(state).await {
        Ok(status) => Outcome::Ok(Response::PostLandStatus(status)),
        Err(error) => Outcome::Error(error),
    }
}

/// Read-only snapshot of post-land checkpoints, per-platform coverage, open
/// bisections and pending inverses.
pub async fn snapshot(state: &Arc<ServerState>) -> Result<PostLandStatus> {
    let state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        let mut answer = None;
        state
            .changes
            .with_read(|conn| {
                answer = Some(read(&state, conn));
                Ok(())
            })
            .map_err(internal)?;
        answer.unwrap_or_else(|| Err(internal("status read did not run")))
    })
    .await
    .map_err(internal)?
}

/// Per-platform checkpoints and live checker coverage for `rosen server status`.
pub async fn platforms_snapshot(state: &Arc<ServerState>) -> Vec<PlatformStatus> {
    snapshot(state)
        .await
        .map(|status| status.platforms)
        .unwrap_or_default()
}

/// Startup installs every table read here. A server that has never run
/// post-land work may still lack some, which reads as nothing recorded.
fn read(state: &ServerState, conn: &Connection) -> Result<PostLandStatus> {
    let Some((name, base)) = project_base(conn)? else {
        return Ok(PostLandStatus {
            base: None,
            last_built: None,
            last_green: None,
            platforms: Vec::new(),
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
    let platforms = read_platforms(state, conn, &name, &base, has("post_land_platform_green")?)?;
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
        platforms,
        sessions,
        pending_inverses,
    })
}

/// Duration a required platform may go without an online checker while
/// checkpoints wait on it before status reports it as waiting (R12, #457).
pub const NO_CHECKER_WAIT_MS: i64 = 60 * 60 * 1000;

fn read_platforms(
    state: &ServerState,
    conn: &Connection,
    name: &str,
    base: &ObjectId,
    has_table: bool,
) -> Result<Vec<PlatformStatus>> {
    let Ok(resolved) = state.project_config.at_commit(base) else {
        return Ok(Vec::new());
    };
    let tables = read_tables(conn, has_table)?;
    let now_ms = super::submitter::now_millis();
    let pre = &resolved.config.checks.pre_land;
    let post = &resolved.config.checks.post_land;
    let mut ordered: Vec<Platform> = Vec::new();
    for platform in pre.platforms.iter().chain(post.platforms.iter()) {
        if !ordered.contains(platform) {
            ordered.push(platform.clone());
        }
    }
    for platform in pre.advisory_platforms().chain(post.advisory_platforms()) {
        if !ordered.contains(platform) {
            ordered.push(platform.clone());
        }
    }
    let mut list = Vec::with_capacity(ordered.len());
    for target in ordered {
        let required_build = pre.platforms.contains(&target);
        let required_test = post.platforms.contains(&target);
        let entry = build_platform_status(
            state,
            conn,
            name,
            &target,
            required_build,
            required_test,
            tables,
            now_ms,
        )?;
        if entry.is_required()
            || entry.last_built.is_some()
            || entry.last_green.is_some()
            || !entry.checkers.is_empty()
        {
            list.push(entry);
        }
    }
    Ok(list)
}

#[allow(clippy::too_many_arguments)]
fn build_platform_status(
    state: &ServerState,
    conn: &Connection,
    name: &str,
    target: &Platform,
    required_build: bool,
    required_test: bool,
    tables: super::platforms::ReadTables,
    now_ms: i64,
) -> Result<PlatformStatus> {
    let key = target.as_str();
    let last_built = if tables.platform_green {
        super::platforms::checkpoint(conn, name, key, CheckKind::Build).map_err(internal)?
    } else {
        None
    };
    let last_green = if tables.platform_green {
        super::platforms::checkpoint(conn, name, key, CheckKind::Test).map_err(internal)?
    } else {
        None
    };
    let checkers = state.checkers.matching(target);
    let waiting_no_checker_since = if (required_build || required_test) && checkers.is_empty() {
        waiting_no_checker(
            state,
            conn,
            name,
            target,
            required_build,
            required_test,
            last_built.as_ref(),
            last_green.as_ref(),
            tables,
            now_ms,
        )?
    } else {
        None
    };
    Ok(PlatformStatus {
        platform: key.to_owned(),
        last_built,
        last_green,
        required_build,
        required_test,
        checkers,
        waiting_no_checker_since,
    })
}

#[allow(clippy::too_many_arguments)]
fn waiting_no_checker(
    state: &ServerState,
    conn: &Connection,
    name: &str,
    target: &Platform,
    required_build: bool,
    required_test: bool,
    last_built: Option<&rosen_protocol::Checkpoint>,
    last_green: Option<&rosen_protocol::Checkpoint>,
    tables: super::platforms::ReadTables,
    now_ms: i64,
) -> Result<Option<i64>> {
    let fallback = state.checkers.started_at_ms();
    let mut oldest_waiting: Option<i64> = None;
    for (required, kind) in [
        (required_build, CheckKind::Build),
        (required_test, CheckKind::Test),
    ] {
        if !required {
            continue;
        }
        if let Some(at) =
            super::platforms::oldest_waiting_at(conn, name, target.as_str(), kind, tables, fallback)
                .map_err(internal)?
        {
            oldest_waiting = Some(oldest_waiting.map_or(at, |cur| cur.min(at)));
        }
    }
    let Some(waiting_at) = oldest_waiting else {
        return Ok(None);
    };
    let last_online = [
        state.checkers.last_offline_for(target),
        last_built.and_then(|cp| cp.at),
        last_green.and_then(|cp| cp.at),
    ]
    .into_iter()
    .flatten()
    .max();
    let wait_elapsed = now_ms.saturating_sub(waiting_at);
    let offline_elapsed = last_online.map_or(wait_elapsed, |at| now_ms.saturating_sub(at));
    if wait_elapsed >= NO_CHECKER_WAIT_MS && offline_elapsed >= NO_CHECKER_WAIT_MS {
        Ok(Some(last_online.unwrap_or(waiting_at)))
    } else {
        Ok(None)
    }
}

fn read_tables(
    conn: &Connection,
    has_platform_green: bool,
) -> Result<super::platforms::ReadTables> {
    let bisections = table_exists(conn, "bisections")?;
    let failures = table_exists(conn, "post_land_failures")?;
    Ok(super::platforms::ReadTables {
        base_history: table_exists(conn, "base_history")?,
        platform_green: has_platform_green,
        bisections,
        bisections_platform: bisections
            && super::submitter::has_column(conn, "bisections", "platform").map_err(internal)?,
        failures,
        failures_platform: failures
            && super::submitter::has_column(conn, "post_land_failures", "platform")
                .map_err(internal)?,
        events: table_exists(conn, "events")?,
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
    let has_platform =
        super::submitter::has_column(conn, "post_land_failures", "platform").map_err(internal)?;
    let platform_col = if has_platform { "f.platform" } else { "''" };
    let mut query = conn
        .prepare(&format!(
            "SELECT f.commit_id,f.change_id,f.kind,{platform_col},r.base,r.paths,r.overlapping,r.reason,
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
                platform: r.get(3)?,
                base: r.get(4)?,
                paths: r.get(5)?,
                overlapping: r.get(6)?,
                reason: r.get(7)?,
                first_refused_at: r.get(8)?,
            })
        })
        .map_err(internal)?;
    rows.map(|row| row.map_err(internal)?.inverse()).collect()
}

struct Row {
    culprit: String,
    change: String,
    kind: String,
    platform: String,
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
            platform: (!self.platform.is_empty()).then_some(self.platform),
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
