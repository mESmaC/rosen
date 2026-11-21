//! Persistent Collapse feed identity and reset handling (SPEC R1, R9, R13, #445).
//!
//! Each Rosen server data directory keeps one random `feed_id` in SQLite and
//! sends it with every state-feed and ledger-push batch. When Collapse sees a
//! different ID it answers HTTP 409 `source_changed`, and even before Collapse
//! checks `feed_id` Rosen treats a remote `highest_sequence` ahead of its own
//! local log as diverged so colliding lines are never sent silently.
//!
//! `rosen collapse link --reset` calls `POST <collapse>/rosen/state/reset`
//! with a fresh `feed_id`, persists that ID, resets local feed cursors, clears
//! diverged health state and wakes both push tasks.

use std::path::Path;
use std::sync::Arc;

use rosen_protocol::{CollapseResetReport, ErrorBody, ErrorCode, EventKind, Outcome, Response};
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

use crate::collapse_health::Feed;
use crate::ledger_push::send::{self, SendFailure};
use crate::ledger_push::Held;
use crate::ServerState;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS collapse_feed (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    feed_id TEXT    NOT NULL
);
";

/// Generates a random 128-bit lowercase hex feed ID.
pub fn generate_feed_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|err| format!("generating feed_id: {err}"))?;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    Ok(out)
}

/// Returns this server's persistent random `feed_id`, creating and persisting
/// one on first call. Concurrent callers always receive the same persisted ID.
pub fn get_or_create(metadata_path: &Path) -> rusqlite::Result<String> {
    let conn = crate::metadata::open(metadata_path)?;
    conn.execute_batch(SCHEMA)?;
    if let Some(existing) = read_existing(&conn)? {
        return Ok(existing);
    }
    let fresh = generate_feed_id().map_err(rusqlite::Error::InvalidParameterName)?;
    conn.execute(
        "INSERT OR IGNORE INTO collapse_feed (id, feed_id) VALUES (1, ?1)",
        [&fresh],
    )?;
    read_existing(&conn)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// Reads the persisted `feed_id` on a read-only connection, returning `None`
/// if none has been created yet.
pub fn read_only(conn: &Connection) -> Result<Option<String>, String> {
    optional_table(read_existing(conn))
}

fn read_existing(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT feed_id FROM collapse_feed WHERE id = 1",
        [],
        |row| row.get(0),
    )
    .optional()
}

/// Highest sequence number stored in this server's local `events` table, or 0
/// when empty or not yet created.
pub fn local_event_seq(conn: &Connection) -> Result<u64, String> {
    let value: Option<i64> = optional_table(
        conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
            row.get(0)
        })
        .optional(),
    )?;
    Ok(value.and_then(|v| u64::try_from(v).ok()).unwrap_or(0))
}

/// Highest sequence number stored in each AIRC ledger locally, or 0 when empty.
pub fn local_ledger_seq(conn: &Connection) -> Result<Held, String> {
    Ok(Held {
        discovery: max_ledger_seq(conn, "discovery")?,
        past_work: max_ledger_seq(conn, "past_work")?,
    })
}

fn max_ledger_seq(conn: &Connection, ledger: &str) -> Result<u64, String> {
    let value: Option<i64> = optional_table(
        conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM channel_ledger WHERE ledger = ?1",
            [ledger],
            |row| row.get(0),
        )
        .optional(),
    )?;
    Ok(value.and_then(|v| u64::try_from(v).ok()).unwrap_or(0))
}

fn optional_table<T>(result: rusqlite::Result<Option<T>>) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.to_string().contains("no such table") => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Collapse's reset endpoint for a repository's `rosen_*` feed tables.
pub fn reset_url(collapse_root: &str) -> String {
    format!("{}/rosen/state/reset", collapse_root.trim_end_matches('/'))
}

/// JSON request body POSTed to `<collapse>/rosen/state/reset`.
pub fn reset_body(feed_id: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "feed_id": feed_id })).unwrap_or_default()
}

/// Operator-only RPC handler for `Request::CollapseReset` (`rosen collapse link --reset`).
pub async fn handle_reset(state: &Arc<ServerState>) -> Outcome {
    match run_reset(state).await {
        Ok(report) => Outcome::Ok(Response::CollapseReset(report)),
        Err(error) => Outcome::Error(error),
    }
}

async fn run_reset(state: &Arc<ServerState>) -> Result<CollapseResetReport, ErrorBody> {
    if !crate::mirror::owner::is_owner_async(state.config.data_dir.clone()).await {
        return Err(ErrorBody::new(
            ErrorCode::Refused,
            "this server is not marked as the mirror owner; start rosen-server with --mirror-owner before resetting Collapse",
        ));
    }
    let (collapse_root, state_endpoint, ledger_endpoint) = resolve_reset_targets(state).await?;
    let new_feed_id = generate_feed_id().map_err(|err| ErrorBody::new(ErrorCode::Internal, err))?;
    post_reset(state, &collapse_root, &state_endpoint, &new_feed_id).await?;
    commit_reset_state(
        state,
        &state_endpoint,
        ledger_endpoint.as_deref(),
        &new_feed_id,
    )
    .await?;
    Ok(CollapseResetReport {
        collapse: collapse_root,
        feed_id: new_feed_id,
    })
}

async fn resolve_reset_targets(
    state: &Arc<ServerState>,
) -> Result<(String, String, Option<String>), ErrorBody> {
    let config = crate::ledger_push::in_force(&state.config)
        .await
        .map_err(|err| ErrorBody::new(ErrorCode::Internal, err))?;
    let Some(config) = config else {
        return Err(ErrorBody::new(
            ErrorCode::Refused,
            "no project base imported yet; import the repository first",
        ));
    };
    let Some(collapse_root) = config.collapse.clone() else {
        return Err(ErrorBody::new(
            ErrorCode::Refused,
            "no `collapse` root is configured on the base; land rosen.toml first (pass --land) before --reset",
        ));
    };
    let state_endpoint = config
        .state_feed_url()
        .expect("collapse root produces state_feed_url");
    Ok((collapse_root, state_endpoint, config.ledger_push_url()))
}

async fn post_reset(
    state: &ServerState,
    collapse_root: &str,
    state_endpoint: &str,
    new_feed_id: &str,
) -> Result<(), ErrorBody> {
    let url = reset_url(collapse_root);
    let mut credential = send::credential(&state.config.data_dir, state_endpoint).await;
    if credential.is_none() {
        credential = send::credential(&state.config.data_dir, &url).await;
    }
    let body = reset_body(new_feed_id);
    let scratch = state.config.data_dir.join("state-push");
    send::post_json(&scratch, &url, &body, credential.as_ref())
        .await
        .map(|_| ())
        .map_err(|failure| map_reset_failure(&url, credential.as_ref(), failure))
}

async fn commit_reset_state(
    state: &ServerState,
    state_endpoint: &str,
    ledger_endpoint: Option<&str>,
    new_feed_id: &str,
) -> Result<(), ErrorBody> {
    let metadata = state.config.metadata_path();
    let se = state_endpoint.to_owned();
    let le = ledger_endpoint.map(str::to_owned);
    let fid = new_feed_id.to_owned();
    tokio::task::spawn_blocking(move || reset_local(&metadata, &se, le.as_deref(), &fid))
        .await
        .map_err(|join| ErrorBody::new(ErrorCode::Internal, join.to_string()))?
        .map_err(|err| ErrorBody::new(ErrorCode::Internal, err.to_string()))?;

    state.collapse.clear_reset(Feed::StateFeed, state_endpoint);
    if let Some(ledger_ep) = ledger_endpoint {
        state.collapse.clear_reset(Feed::LedgerPush, ledger_ep);
    }
    state.events.publish(
        EventKind::StatePush,
        None,
        json!({
            "status": "reset",
            "endpoint": state_endpoint,
            "feed_id": new_feed_id,
        }),
    );
    state.notify_collapse_reset();
    Ok(())
}

fn map_reset_failure(
    url: &str,
    credential: Option<&send::Credential>,
    failure: SendFailure,
) -> ErrorBody {
    match failure {
        SendFailure::Gone => ErrorBody::new(
            ErrorCode::Refused,
            format!("HTTP 410 from {url}: repository is archived or deleted"),
        ),
        SendFailure::SourceChanged { message } => ErrorBody::new(
            ErrorCode::Refused,
            format!("HTTP 409 from {url}: {message}"),
        ),
        SendFailure::Failed { status, message } => {
            let detail = match (credential, status) {
                (None, Some(401 | 403)) if url.starts_with("https://") => {
                    format!("{message} (git's credential helper holds no token for {url})")
                }
                _ => message,
            };
            let code = if matches!(status, Some(401 | 403)) {
                ErrorCode::Refused
            } else {
                ErrorCode::Internal
            };
            ErrorBody::new(code, format!("resetting Collapse feed at {url}: {detail}"))
        }
    }
}

/// Persists `new_feed_id` and resets local state-feed and ledger-push cursors
/// in one SQLite transaction so the next pass resends all live documents and
/// lines from sequence 0.
fn reset_local(
    metadata_path: &Path,
    state_endpoint: &str,
    ledger_endpoint: Option<&str>,
    new_feed_id: &str,
) -> rusqlite::Result<()> {
    let mut conn = crate::metadata::open(metadata_path)?;
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS state_push (
             endpoint  TEXT    NOT NULL PRIMARY KEY,
             scanned   INTEGER NOT NULL,
             delivered INTEGER NOT NULL
         ) WITHOUT ROWID;",
    )?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO collapse_feed (id, feed_id) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET feed_id = ?1",
        [new_feed_id],
    )?;
    // Explicitly set scanned=0, delivered=0 so pre-reset event/channel lines
    // on a fresh data directory are sent instead of skipped at the log tip.
    tx.execute(
        "INSERT INTO state_push (endpoint, scanned, delivered) VALUES (?1, 0, 0)
         ON CONFLICT (endpoint) DO UPDATE SET scanned = 0, delivered = 0",
        [state_endpoint],
    )?;
    delete_if_table_exists(&tx, "state_push_sent", state_endpoint)?;
    delete_if_table_exists(&tx, "state_push_refused", state_endpoint)?;
    if let Some(ledger_ep) = ledger_endpoint {
        delete_if_table_exists(&tx, "ledger_push", ledger_ep)?;
        delete_if_table_exists(&tx, "ledger_push_refused", ledger_ep)?;
    }
    tx.commit()
}

fn delete_if_table_exists(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    endpoint: &str,
) -> rusqlite::Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )?;
    if exists {
        let sql = format!("DELETE FROM {table} WHERE endpoint = ?1");
        tx.execute(&sql, [endpoint])?;
    }
    Ok(())
}
