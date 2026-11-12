//! The event log and subscriber cursors in the metadata store (SPEC R9, D13).
//!
//! Events are append-only rows; the sequence number is the row id, so it
//! keeps counting across server restarts. Each targeted event lists the
//! agents it concerns in `event_audience`, keyed by agent first so one
//! subscriber's catch-up is a range scan. A broadcast event (one about no
//! change, such as `mirror`) reaches every subscriber and has no audience rows.

use rosen_protocol::{Event, EventKind};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// Creates the event tables if they are missing. Safe to run on every open.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS events (
             seq        INTEGER PRIMARY KEY,
             kind       TEXT    NOT NULL,
             change_id  TEXT,
             detail     TEXT    NOT NULL,
             broadcast  INTEGER NOT NULL CHECK (broadcast IN (0, 1)),
             created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS event_audience (
             agent TEXT    NOT NULL,
             seq   INTEGER NOT NULL REFERENCES events(seq),
             PRIMARY KEY (agent, seq)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS event_cursors (
             subscriber TEXT    PRIMARY KEY,
             seq        INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         ) WITHOUT ROWID;",
    )
}

/// The newest stored sequence number, 0 when the log is empty.
pub fn last_seq(conn: &Connection) -> rusqlite::Result<u64> {
    let max: Option<i64> = conn.query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))?;
    Ok(max.map_or(0, to_seq))
}

/// Appends one event in a single transaction with its audience. `None` as
/// the audience broadcasts it. Returns the new sequence number.
pub fn append(
    conn: &mut Connection,
    kind: EventKind,
    change: Option<&str>,
    detail: &serde_json::Value,
    audience: Option<&BTreeSet<String>>,
) -> rusqlite::Result<u64> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO events (kind, change_id, detail, broadcast, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            kind_text(kind),
            change,
            detail.to_string(),
            audience.is_none(),
            now_millis()
        ],
    )?;
    // Rows are never deleted, so the row id is always one past the newest.
    let seq = tx.last_insert_rowid();
    for agent in audience.into_iter().flatten() {
        tx.execute(
            "INSERT OR IGNORE INTO event_audience (agent, seq) VALUES (?1, ?2)",
            params![agent, seq],
        )?;
    }
    tx.commit()?;
    Ok(to_seq(seq))
}

/// Up to `limit` events after `cursor`, oldest first. With a subscriber,
/// only broadcasts and the events whose audience names it.
pub fn page(
    conn: &Connection,
    cursor: u64,
    subscriber: Option<&str>,
    limit: u64,
) -> rusqlite::Result<Vec<Event>> {
    let cursor = i64::try_from(cursor).unwrap_or(i64::MAX);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let columns = "SELECT seq, kind, change_id, detail FROM events";
    let rows = match subscriber {
        None => {
            let sql = format!("{columns} WHERE seq > ?1 ORDER BY seq LIMIT ?2");
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![cursor, limit], read_event)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        }
        Some(agent) => {
            let sql = format!(
                "{columns} WHERE seq > ?1 AND (broadcast = 1 OR seq IN (
                     SELECT seq FROM event_audience WHERE agent = ?2 AND seq > ?1))
                 ORDER BY seq LIMIT ?3"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![cursor, agent, limit], read_event)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        }
    };
    Ok(rows.into_iter().flatten().collect())
}

/// The subscriber's stored cursor, if it ever acknowledged an event.
pub fn cursor(conn: &Connection, subscriber: &str) -> rusqlite::Result<Option<u64>> {
    conn.query_row(
        "SELECT seq FROM event_cursors WHERE subscriber = ?1",
        [subscriber],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map(|seq| seq.map(to_seq))
}

/// Moves the subscriber's cursor to `seq`, never backwards, so a late or
/// repeated acknowledgement cannot replay events already handled. Returns
/// the cursor now stored.
pub fn acknowledge(conn: &Connection, subscriber: &str, seq: u64) -> rusqlite::Result<u64> {
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    conn.query_row(
        "INSERT INTO event_cursors (subscriber, seq, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (subscriber) DO UPDATE
             SET seq = max(seq, excluded.seq), updated_at = excluded.updated_at
         RETURNING seq",
        params![subscriber, seq, now_millis()],
        |row| row.get::<_, i64>(0),
    )
    .map(to_seq)
}

/// Reads one row. A row whose kind this build does not know (written by a
/// newer server) is skipped rather than failing the whole page.
fn read_event(row: &Row) -> rusqlite::Result<Option<Event>> {
    let seq: i64 = row.get(0)?;
    let kind: String = row.get(1)?;
    let change: Option<String> = row.get(2)?;
    let detail: String = row.get(3)?;
    let Ok(kind) = serde_json::from_value::<EventKind>(serde_json::Value::String(kind)) else {
        return Ok(None);
    };
    let detail = serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null);
    Ok(Some(Event {
        seq: to_seq(seq),
        kind,
        change,
        detail,
    }))
}

fn kind_text(kind: EventKind) -> String {
    match serde_json::to_value(kind) {
        Ok(serde_json::Value::String(text)) => text,
        _ => format!("{kind:?}").to_lowercase(),
    }
}

fn to_seq(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}
