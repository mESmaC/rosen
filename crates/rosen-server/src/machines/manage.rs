//! Management never deletes a machine's work or permits a revoked credential again.
use rosen_protocol::machine::{JoinKeyRecord, MachineRecord, MachineRole, MachineStatus};
use rosen_protocol::{ErrorBody, ErrorCode};
use rusqlite::{Connection, OptionalExtension, Row};
pub type Presence = std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>>;

pub async fn handle(
    state: &std::sync::Arc<crate::ServerState>,
    work: impl FnOnce(&Connection) -> Result<rosen_protocol::Response, ErrorBody> + Send + 'static,
) -> rosen_protocol::Outcome {
    let state = std::sync::Arc::clone(state);
    match tokio::task::spawn_blocking(move || {
        let conn = super::open(&state.config.data_dir).map_err(database)?;
        let mut response = work(&conn)?;
        if let rosen_protocol::Response::Machines(records) = &mut response {
            let agents = state
                .machine_agents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for record in records {
                record.running_agents = agents
                    .get(&record.id)
                    .map(|nicks| nicks.keys().cloned().collect())
                    .unwrap_or_default();
            }
        }
        Ok::<_, ErrorBody>(response)
    })
    .await
    {
        Ok(Ok(response)) => rosen_protocol::Outcome::Ok(response),
        Ok(Err(error)) => rosen_protocol::Outcome::Error(error),
        Err(error) => {
            rosen_protocol::Outcome::Error(ErrorBody::new(ErrorCode::Internal, error.to_string()))
        }
    }
}

/// Count subscriptions rather than persisting presence, which must vanish on crash.
pub fn join(state: &crate::ServerState, credential: &str, nick: &str) -> Option<(String, String)> {
    let conn = super::open(&state.config.data_dir).ok()?;
    let id: String = conn
        .query_row(
            "SELECT id FROM machines WHERE credential_hash=?1 AND status='active'",
            [super::keys::digest(credential)],
            |r| r.get(0),
        )
        .ok()?;
    let mut agents = state
        .machine_agents
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *agents
        .entry(id.clone())
        .or_default()
        .entry(nick.into())
        .or_default() += 1;
    Some((id, nick.into()))
}

pub fn leave(state: &crate::ServerState, machine: &str, nick: &str) {
    let mut agents = state
        .machine_agents
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(nicks) = agents.get_mut(machine) {
        if let Some(count) = nicks.get_mut(nick) {
            *count -= 1;
            if *count == 0 {
                nicks.remove(nick);
            }
        }
        if nicks.is_empty() {
            agents.remove(machine);
        }
    }
}

fn database(error: rusqlite::Error) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, error.to_string())
}

fn machine(row: &Row<'_>) -> rusqlite::Result<MachineRecord> {
    let role: String = row.get(2)?;
    let status: String = row.get(3)?;
    let agents: String = row.get(5)?;
    Ok(MachineRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        role: if role == "checker" {
            MachineRole::Checker
        } else {
            MachineRole::Agent
        },
        status: match status.as_str() {
            "active" => MachineStatus::Active,
            "pending" => MachineStatus::Pending,
            _ => MachineStatus::Revoked,
        },
        last_contact: row.get(4)?,
        running_agents: serde_json::from_str(&agents).unwrap_or_default(),
    })
}

pub fn list(conn: &Connection) -> Result<Vec<MachineRecord>, ErrorBody> {
    let mut query = conn
        .prepare("SELECT id,name,role,status,last_contact,running_agents FROM machines ORDER BY id")
        .map_err(database)?;
    let rows = query.query_map([], machine).map_err(database)?;
    rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(database)
}

fn get(conn: &Connection, id: &str) -> Result<MachineRecord, ErrorBody> {
    conn.query_row(
        "SELECT id,name,role,status,last_contact,running_agents FROM machines WHERE id=?1",
        [id],
        machine,
    )
    .optional()
    .map_err(database)?
    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, format!("no machine {id:?}")))
}

/// Activates a pending machine. Its credential lasts 30 days from approval,
/// not from the request (R14): a machine approved days after it asked would
/// otherwise get a shortened credential, or an already expired one that
/// neither authenticates nor rotates. The machine's daemon still holds the
/// request-time expiry, so it simply rotates early.
pub fn approve(conn: &Connection, id: &str) -> Result<MachineRecord, ErrorBody> {
    let expires_at = super::now() + super::credentials::LIFETIME;
    conn.execute(
        "UPDATE machines SET status='active', expires_at=?2 WHERE id=?1 AND status='pending'",
        rusqlite::params![id, expires_at],
    )
    .map_err(database)?;
    let record = get(conn, id)?;
    if record.status != MachineStatus::Active {
        return Err(ErrorBody::new(
            ErrorCode::Refused,
            "a revoked machine must enroll again",
        ));
    }
    Ok(record)
}

pub fn revoke(conn: &Connection, id: &str) -> Result<MachineRecord, ErrorBody> {
    conn.execute("UPDATE machines SET status='revoked' WHERE id=?1", [id])
        .map_err(database)?;
    get(conn, id)
}

fn key(row: &Row<'_>) -> rusqlite::Result<JoinKeyRecord> {
    let role: String = row.get(3)?;
    Ok(JoinKeyRecord {
        id: row.get(0)?,
        expires_at: row.get(1)?,
        remaining: row.get(2)?,
        role: if role == "checker" {
            MachineRole::Checker
        } else {
            MachineRole::Agent
        },
    })
}

pub fn keys(conn: &Connection) -> Result<Vec<JoinKeyRecord>, ErrorBody> {
    let mut query = conn
        .prepare("SELECT id,expires_at,remaining,role FROM join_keys ORDER BY id")
        .map_err(database)?;
    let rows = query.query_map([], key).map_err(database)?;
    rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(database)
}

pub fn revoke_key(conn: &Connection, id: &str) -> Result<JoinKeyRecord, ErrorBody> {
    conn.execute("UPDATE join_keys SET remaining=0 WHERE id=?1", [id])
        .map_err(database)?;
    conn.query_row(
        "SELECT id,expires_at,remaining,role FROM join_keys WHERE id=?1",
        [id],
        key,
    )
    .optional()
    .map_err(database)?
    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, format!("no join key {id:?}")))
}
