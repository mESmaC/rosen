//! The machine list (R14) and channel presence (R13) as feed documents
//! (#406).
//!
//! Both are read with plain queries that select only what Collapse shows,
//! so no credential, credential hash, join key, network address or
//! certificate fingerprint can reach a batch. Each is fingerprinted
//! without its clock fields, so the passage of time alone never sends it.

use std::path::Path;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::unit::{Delivery, Section, Unit};
use crate::machines::manage::Presence;

/// How long without contact before a machine shows as out of contact.
const OUT_OF_CONTACT: u64 = 5 * 60;

/// The full machine list. `running` is the server's in-memory count of
/// subscribed agents per machine; `now` is Unix seconds. Reads SQLite: call
/// it on the blocking pool.
pub fn machines(metadata: &Path, running: &Presence, now: u64) -> rusqlite::Result<Unit> {
    let conn = crate::metadata::open(metadata)?;
    let mut listed = Vec::new();
    let mut basis = Vec::new();
    if has_table(&conn, "machines")? {
        for row in machine_rows(&conn, now)? {
            let agents: Vec<&String> = running
                .get(&row.id)
                .map(|nicks| nicks.keys().collect())
                .unwrap_or_default();
            let (item, meaning) = machine_item(&row, agents, now);
            listed.push(item);
            basis.push(meaning);
        }
    }
    let item = json!({ "machines": listed });
    let basis = json!({ "machines": basis });
    Ok(Unit::with_basis(
        "machines".to_owned(),
        vec![(Section::Machines, item)],
        &basis,
    ))
}

/// The members joined now. With `refresh`, and anyone joined, it goes even
/// unchanged, so the endpoint sees how current its count is. `now_ms` is
/// Unix milliseconds. Reads SQLite: call it on the blocking pool.
pub fn presence(metadata: &Path, now_ms: i64, refresh: bool) -> rusqlite::Result<Unit> {
    let conn = crate::metadata::open(metadata)?;
    let members = if has_table(&conn, "channel_members")? {
        joined(&conn)?
    } else {
        Vec::new()
    };
    let delivery = if refresh && !members.is_empty() {
        Delivery::Refresh
    } else {
        Delivery::Document
    };
    let item = json!({ "members": members, "as_of": now_ms });
    let basis = json!({ "members": members });
    let mut unit = Unit::with_basis(
        "presence".to_owned(),
        vec![(Section::Presence, item)],
        &basis,
    );
    unit.delivery = delivery;
    Ok(unit)
}

struct MachineRow {
    id: String,
    name: String,
    role: String,
    status: String,
    last_contact: u64,
    expires_at: u64,
}

/// Every machine but a pending request past its expiry, which `machine
/// list` would remove (R14).
fn machine_rows(conn: &Connection, now: u64) -> rusqlite::Result<Vec<MachineRow>> {
    let expired = now.saturating_sub(crate::machines::pending::EXPIRY);
    let mut statement = conn.prepare_cached(
        "SELECT id, name, role, status, last_contact, expires_at FROM machines
         WHERE NOT (status = 'pending' AND last_contact <= ?1) ORDER BY id",
    )?;
    let rows = statement.query_map(params![expired], |row| {
        Ok(MachineRow {
            id: row.get(0)?,
            name: row.get(1)?,
            role: row.get(2)?,
            status: row.get(3)?,
            last_contact: row.get(4)?,
            expires_at: row.get(5)?,
        })
    })?;
    rows.collect()
}

/// A machine's item and the part of it whose change is news: everything
/// but `last_contact`, which is rounded to the minute and only counts when
/// it crosses [`OUT_OF_CONTACT`].
fn machine_item(row: &MachineRow, agents: Vec<&String>, now: u64) -> (Value, Value) {
    let active = row.status == "active";
    let pending = row.status == "pending";
    // As `machine list` reports it: only an active machine's live
    // credential has an expiry, and a pending one's contact is its request.
    let expires = (active && row.expires_at > now).then_some(row.expires_at);
    let requested = pending.then_some(row.last_contact);
    let out_of_contact = active && now.saturating_sub(row.last_contact) >= OUT_OF_CONTACT;
    let meaning = json!({
        "id": row.id,
        "name": row.name,
        "role": row.role,
        "status": row.status,
        "running_agents": agents,
        "credential_expires_at": expires,
        "requested_at": requested,
        "out_of_contact": out_of_contact,
    });
    let mut item = meaning.clone();
    if let Value::Object(map) = &mut item {
        map.insert("last_contact".into(), json!(row.last_contact / 60 * 60));
    }
    (item, meaning)
}

fn joined(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut statement = conn.prepare_cached(
        "SELECT nick, machine, mode, joined_at FROM channel_members
         WHERE left_at IS NULL ORDER BY nick, member",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(json!({
            "nick": row.get::<_, String>(0)?,
            "machine": row.get::<_, String>(1)?,
            "mode": row.get::<_, String>(2)?,
            "joined_at": row.get::<_, i64>(3)?,
        }))
    })?;
    rows.collect()
}

fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )
}
