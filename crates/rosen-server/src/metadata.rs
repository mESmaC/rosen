//! Shared SQLite metadata initialization (SPEC.md §3, D13).
//!
//! Keep module-owned DDL in its module, but install the authoritative schema
//! together so the first service to open the database cannot leave half a schema.

use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::path::Path;
use std::time::Duration;

pub const SCHEMA_VERSION: i64 = 1;

/// Opens an existing metadata database or creates one at `path`.
/// The caller creates its containing directory and handles filesystem errors.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    configure(&connection)?;
    ensure_schema(&connection)?;
    Ok(connection)
}

/// Connection settings are local to each handle, even when WAL is persistent.
pub fn configure(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    // A committed ref or claim must survive a power failure, not just a crash.
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")
}

/// Adopts pre-versioned databases without changing or deleting their rows.
/// A newer schema is refused rather than opened by an older server.
pub fn ensure_schema(connection: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "metadata schema {version} is newer than supported version {SCHEMA_VERSION}"
        )));
    }
    crate::changes::schema::ensure_change_schema(&tx)?;
    crate::conflicts::ensure_schema(&tx)?;
    crate::claims::ensure_schema(&tx)?;
    crate::reviews::ensure_schema(&tx)?;
    crate::landing::ensure_schema(&tx)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}
