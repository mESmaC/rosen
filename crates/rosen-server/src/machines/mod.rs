//! Transactional enrollment authority (SPEC R14).
pub mod credentials;
pub mod keys;
pub mod manage;
pub mod signing;
use crate::ServerState;
use rosen_protocol::{CreateJoinKey, ErrorBody, ErrorCode, Outcome, Response};
use rusqlite::Connection;
use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn open(data_dir: &Path) -> rusqlite::Result<Connection> {
    let conn = crate::metadata::open(&data_dir.join("metadata.sqlite"))?;
    ensure_schema(&conn)?;
    Ok(conn)
}
/// The landing ledger reads machine status when it verifies a receipt, so
/// it installs these tables in its own database too.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS machines (
        id TEXT PRIMARY KEY, name TEXT NOT NULL, role TEXT NOT NULL, status TEXT NOT NULL,
        last_contact INTEGER NOT NULL, running_agents TEXT NOT NULL DEFAULT '[]',
        credential_hash TEXT NOT NULL, expires_at INTEGER NOT NULL,
        previous_hash TEXT NOT NULL DEFAULT '', previous_expires INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS join_keys (id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL,
        expires_at INTEGER NOT NULL, remaining INTEGER NOT NULL, role TEXT NOT NULL);",
    )
}
/// How stale a machine's recorded last contact may get, in seconds. Every
/// network request authenticates, so writing it each time took the metadata
/// write lock once per frame, contending with landing (#156).
const LAST_CONTACT_EVERY: u64 = 60;

/// Whether `secret` is an active, unexpired machine's credential. A read, so
/// in WAL mode it never waits on a writer; the last-contact record it
/// refreshes is written only when stale and never waited for.
pub fn authenticate(dir: &Path, secret: &str) -> rusqlite::Result<bool> {
    use rusqlite::OptionalExtension;
    let conn = open(dir)?;
    let now = now();
    let found: Option<(String, u64)> = conn
        .query_row(
            "SELECT id,last_contact FROM machines WHERE credential_hash=?1
            AND status='active' AND expires_at>?2",
            rusqlite::params![keys::digest(secret), now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((id, last_contact)) = found else {
        return Ok(false);
    };
    if now.saturating_sub(last_contact) >= LAST_CONTACT_EVERY {
        // Only a record of the visit: a busy database skips it until a later
        // request, rather than delaying or refusing a valid machine.
        conn.busy_timeout(std::time::Duration::ZERO)?;
        let _ = conn.execute(
            "UPDATE machines SET last_contact=?1 WHERE id=?2 AND last_contact<?1",
            rusqlite::params![now, id],
        );
    }
    Ok(true)
}
/// The active, unexpired machine a credential belongs to, if any.
pub fn machine_of(dir: &Path, secret: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    open(dir)?
        .query_row(
            "SELECT id FROM machines WHERE credential_hash=?1 AND status='active' AND expires_at>?2",
            rusqlite::params![keys::digest(secret), now()],
            |r| r.get(0),
        )
        .optional()
}
async fn run(
    state: &Arc<ServerState>,
    work: impl FnOnce(&ServerState) -> Result<Response, String> + Send + 'static,
) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || work(&state)).await {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(ErrorCode::Refused, err)),
        Err(err) => Outcome::Error(ErrorBody::new(ErrorCode::Refused, err.to_string())),
    }
}
pub async fn create(state: &Arc<ServerState>, req: CreateJoinKey) -> Outcome {
    run(state, move |state| {
        keys::create(state, req).map(|key| Response::JoinKeyCreated { key })
    })
    .await
}
pub async fn enroll(state: &Arc<ServerState>, key: String, name: String) -> Outcome {
    run(state, move |state| {
        credentials::enroll(&state.config.data_dir, &key, &name).map(Response::MachineCredential)
    })
    .await
}
pub async fn rotate(state: &Arc<ServerState>, credential: String) -> Outcome {
    run(state, move |state| {
        credentials::rotate(&state.config.data_dir, &credential).map(Response::MachineCredential)
    })
    .await
}

pub async fn pending(
    state: &Arc<ServerState>,
    name: String,
    role: rosen_protocol::MachineRole,
) -> Outcome {
    run(state, move |state| {
        credentials::pending(&state.config.data_dir, &name, role).map(Response::MachineCredential)
    })
    .await
}
