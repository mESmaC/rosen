//! Events staged in the transaction that commits their cause (SPEC R9).
//!
//! A producer commits its change on the change registry's connection, and
//! the log appends on its own. Publishing after the commit loses the event
//! if the server stops between the two (#118). A producer instead stages the
//! event in `event_outbox` inside its own transaction, so the cause and its
//! event commit or roll back together, then calls
//! [`super::EventLog::deliver_staged`] once it has committed.
//!
//! Delivery deletes the outbox row in the transaction that appends the
//! event, so each staged event is stored exactly once even when two
//! deliveries race. Startup delivers whatever a stopped server left staged.

use super::{store, Concern};
use rosen_core::AgentId;
use rosen_protocol::EventKind;
use rusqlite::{params, Connection};

/// One event to stage with its cause.
#[derive(Debug, Clone)]
pub struct Staged {
    pub kind: EventKind,
    /// The change it is about, or `None` to broadcast it.
    pub change: Option<String>,
    pub detail: serde_json::Value,
    /// Agents and paths it concerns beyond what the change records.
    pub concern: Concern,
}

impl Staged {
    /// An event about `change` with no audience beyond the change's own.
    pub fn about(kind: EventKind, change: impl ToString, detail: serde_json::Value) -> Self {
        Staged {
            kind,
            change: Some(change.to_string()),
            detail,
            concern: Concern::default(),
        }
    }
}

/// Creates the outbox if it is missing. Installed with the metadata schema,
/// since producers stage on the registry's connection, not the log's.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS event_outbox (
             id        INTEGER PRIMARY KEY AUTOINCREMENT,
             kind      TEXT NOT NULL,
             change_id TEXT,
             detail    TEXT NOT NULL,
             agents    TEXT NOT NULL,
             paths     TEXT NOT NULL
         );",
    )
}

/// Stages `event` inside the caller's open transaction. Call it before the
/// commit that makes the event's cause durable.
pub fn stage(conn: &Connection, event: &Staged) -> rusqlite::Result<()> {
    let agents: Vec<&str> = event.concern.agents.iter().map(AgentId::as_str).collect();
    conn.execute(
        "INSERT INTO event_outbox (kind, change_id, detail, agents, paths)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            store::kind_text(event.kind),
            event.change,
            event.detail.to_string(),
            serde_json::Value::from(agents).to_string(),
            serde_json::Value::from(event.concern.paths.clone()).to_string(),
        ],
    )?;
    Ok(())
}

/// Every staged event, oldest first. A row whose kind this build does not
/// know stays staged for the newer server that wrote it.
pub(super) fn staged(conn: &Connection) -> rusqlite::Result<Vec<(i64, Staged)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, kind, change_id, detail, agents, paths FROM event_outbox ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (id, kind, change, detail, agents, paths) = row?;
        let Ok(kind) = serde_json::from_value(serde_json::Value::String(kind)) else {
            continue;
        };
        let concern = Concern {
            // An unreadable list only narrows the audience the change's own
            // records already give; the event itself is never dropped.
            agents: parse_list(&agents)
                .into_iter()
                .filter_map(|agent| AgentId::new(agent).ok())
                .collect(),
            paths: parse_list(&paths),
        };
        let detail = serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null);
        let staged = Staged {
            kind,
            change,
            detail,
            concern,
        };
        events.push((id, staged));
    }
    Ok(events)
}

/// Removes staged row `id` inside the caller's transaction. False when
/// another delivery already moved it, so the caller must not append it again.
pub(super) fn take(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM event_outbox WHERE id = ?1", params![id])? == 1)
}

fn parse_list(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}
