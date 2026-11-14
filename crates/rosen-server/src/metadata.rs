//! Shared SQLite metadata initialization (SPEC.md §3, D13).
//!
//! Keep module-owned DDL in its module, but install the authoritative schema
//! together so the first service to open the database cannot leave half a schema.

use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

pub const SCHEMA_VERSION: i64 = 1;

/// Databases this process has already brought to the current schema.
/// Installing it takes the write lock and an fsync'd commit, so doing it on
/// every open made each history query wait behind a land, then fail as busy
/// (issue #120). Modules still migrate within one `SCHEMA_VERSION`, so the
/// first open in each process always installs.
static INSTALLED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Opens an existing metadata database or creates one at `path`.
/// The caller creates its containing directory and handles filesystem errors.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    configure(&connection)?;
    if !installed(path, &connection)? {
        ensure_schema(&connection)?;
        let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
        installed.insert(path.to_path_buf());
    }
    Ok(connection)
}

/// Whether this process installed the schema at `path` and the file there
/// still carries it. A database deleted and created again at the same path
/// reads as version 0, and one a newer server upgraded reads higher; both go
/// through [`ensure_schema`] again. Reading the version takes no write lock.
fn installed(path: &Path, connection: &Connection) -> rusqlite::Result<bool> {
    let known = INSTALLED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(path);
    if !known {
        return Ok(false);
    }
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(version == SCHEMA_VERSION)
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
    crate::channel::ensure_schema(&tx)?;
    crate::events::ensure_outbox_schema(&tx)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}
