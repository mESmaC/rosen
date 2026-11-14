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
pub fn authenticate(dir: &Path, secret: &str) -> rusqlite::Result<bool> {
    let conn = open(dir)?;
    Ok(conn.execute(
        "UPDATE machines SET last_contact=?1 WHERE credential_hash=?2
        AND status='active' AND expires_at>?1",
        rusqlite::params![now(), keys::digest(secret)],
    )? == 1)
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
