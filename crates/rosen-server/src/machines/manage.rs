//! Management never deletes a machine's work or permits a revoked credential again.
use rosen_protocol::machine::{JoinKeyRecord, MachineRecord, MachineRole, MachineStatus};
use rosen_protocol::{ErrorBody, ErrorCode};
use rusqlite::{Connection, OptionalExtension, Row};

fn database(error: rusqlite::Error) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, error.to_string())
}

fn machine(row: &Row<'_>) -> rusqlite::Result<MachineRecord> {
    let role: String = row.get(2)?;
    let status: String = row.get(3)?;
    let agents: String = row.get(5)?;
    Ok(MachineRecord {
        id: row.get(0)?, name: row.get(1)?,
        role: if role == "checker" { MachineRole::Checker } else { MachineRole::Agent },
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
    let mut query = conn.prepare(
        "SELECT id,name,role,status,last_contact,running_agents FROM machines ORDER BY id",
    ).map_err(database)?;
    query.query_map([], machine).map_err(database)?
        .collect::<Result<Vec<_>, _>>().map_err(database)
}

fn get(conn: &Connection, id: &str) -> Result<MachineRecord, ErrorBody> {
    conn.query_row(
        "SELECT id,name,role,status,last_contact,running_agents FROM machines WHERE id=?1",
        [id], machine,
    ).optional().map_err(database)?.ok_or_else(|| {
        ErrorBody::new(ErrorCode::NotFound, format!("no machine {id:?}"))
    })
}

pub fn approve(conn: &Connection, id: &str) -> Result<MachineRecord, ErrorBody> {
    conn.execute("UPDATE machines SET status='active' WHERE id=?1 AND status='pending'", [id])
        .map_err(database)?;
    let record = get(conn, id)?;
    if record.status != MachineStatus::Active {
        return Err(ErrorBody::new(ErrorCode::Refused, "a revoked machine must enroll again"));
    }
    Ok(record)
}

pub fn revoke(conn: &Connection, id: &str) -> Result<MachineRecord, ErrorBody> {
    conn.execute("UPDATE machines SET status='revoked' WHERE id=?1", [id]).map_err(database)?;
    get(conn, id)
}

fn key(row: &Row<'_>) -> rusqlite::Result<JoinKeyRecord> {
    let role: String = row.get(3)?;
    Ok(JoinKeyRecord {
        id: row.get(0)?, expires_at: row.get(1)?, remaining: row.get(2)?,
        role: if role == "checker" { MachineRole::Checker } else { MachineRole::Agent },
    })
}

pub fn keys(conn: &Connection) -> Result<Vec<JoinKeyRecord>, ErrorBody> {
    let mut query = conn.prepare(
        "SELECT id,expires_at,remaining,role FROM join_keys ORDER BY id",
    ).map_err(database)?;
    query.query_map([], key).map_err(database)?
        .collect::<Result<Vec<_>, _>>().map_err(database)
}

pub fn revoke_key(conn: &Connection, id: &str) -> Result<JoinKeyRecord, ErrorBody> {
    conn.execute("UPDATE join_keys SET remaining=0 WHERE id=?1", [id]).map_err(database)?;
    conn.query_row("SELECT id,expires_at,remaining,role FROM join_keys WHERE id=?1", [id], key)
        .optional().map_err(database)?.ok_or_else(|| {
            ErrorBody::new(ErrorCode::NotFound, format!("no join key {id:?}"))
        })
}
