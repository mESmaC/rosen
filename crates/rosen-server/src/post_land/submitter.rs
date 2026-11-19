//! Who reported a post-land or bisection result, and when (R12, R14 "Agents").
use rosen_core::AgentId;
use rosen_protocol::{ErrorBody, ErrorCode};
use rusqlite::Connection;
use std::time::{SystemTime, UNIX_EPOCH};

/// A result's submitter as the server records it: the request's nick with
/// the machine from the connection's credential, never the request's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    /// `nick@machine`, or the machine alone when the request named no nick.
    pub by: String,
    /// Unix milliseconds the server accepted the result.
    pub at: i64,
}

impl Submitted {
    pub fn new(nick: Option<&AgentId>, machine: &str) -> Result<Self, ErrorBody> {
        let by = match nick {
            // A nick holding `@` could read as a different machine.
            Some(nick) if nick.as_str().contains('@') => {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "submitter {nick} must be a nick without '@'; \
                         the server records its machine"
                    ),
                ))
            }
            Some(nick) => nick
                .on_machine(machine)
                .map_err(|error| ErrorBody::new(ErrorCode::InvalidArgument, error.to_string()))?
                .to_string(),
            None => machine.to_owned(),
        };
        Ok(Self {
            by,
            at: now_millis(),
        })
    }
}

/// The submitter fields every result event carries; null for old results.
pub fn detail(submitted: Option<&Submitted>) -> (serde_json::Value, serde_json::Value) {
    match submitted {
        Some(s) => (s.by.clone().into(), s.at.into()),
        None => (serde_json::Value::Null, serde_json::Value::Null),
    }
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Adds the nullable `submitter` and `submitted_at` columns to `table`.
/// Rows written before them read as having no submitter.
pub fn add_columns(conn: &Connection, table: &str) -> rusqlite::Result<()> {
    add_column(conn, table, "submitter", "TEXT")?;
    add_column(conn, table, "submitted_at", "INTEGER")
}

/// Adds one nullable column if it is missing. Two connections may race to
/// add it; the loser finds it present and succeeds.
pub fn add_column(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> rusqlite::Result<()> {
    if has_column(conn, table, column)? {
        return Ok(());
    }
    match conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}")) {
        Ok(()) => Ok(()),
        Err(_) if has_column(conn, table, column)? => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
        [table, column],
        |r| r.get(0),
    )
}
