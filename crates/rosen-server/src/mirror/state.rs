//! The mirror's row in the metadata store (SPEC D13): the configured remote
//! and the last commit the remote accepted.
//!
//! The pushed commit is stored, not just held in memory, so a server that
//! restarts knows whether the mirror is behind the base and pushes at once.

use std::path::Path;
use std::time::Duration;

use rosen_core::ObjectId;
use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mirror (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    remote TEXT NOT NULL,
    pushed TEXT
);
";

/// Where the base is mirrored, and how far the mirror has got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorTarget {
    /// A URL or local path git can push to.
    pub remote: String,
    /// The base commit the remote last accepted, if any push succeeded.
    pub pushed: Option<ObjectId>,
}

pub struct MirrorStore {
    connection: Connection,
}

impl MirrorStore {
    /// Opens the mirror table in the metadata database at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self { connection })
    }

    /// Names the mirror remote. A new remote starts with nothing pushed, so
    /// the next push sends the whole base.
    pub fn set_remote(&self, remote: &str) -> rusqlite::Result<()> {
        self.connection.execute(
            "INSERT INTO mirror (id, remote, pushed) VALUES (1, ?1, NULL)
             ON CONFLICT (id) DO UPDATE SET remote = ?1, pushed = NULL",
            params![remote],
        )?;
        Ok(())
    }

    /// The configured mirror, or `None` when the project has none.
    pub fn target(&self) -> rusqlite::Result<Option<MirrorTarget>> {
        let row: Option<(String, Option<String>)> = self
            .connection
            .query_row(
                "SELECT remote, pushed FROM mirror WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(remote, pushed)| MirrorTarget {
            remote,
            // A damaged value only costs one redundant push, which the
            // remote answers as already up to date.
            pushed: pushed.and_then(|hex| ObjectId::from_hex(&hex).ok()),
        }))
    }

    /// Records that the remote accepted `commit`, unless the remote changed
    /// since the push began.
    pub fn record_pushed(&self, remote: &str, commit: &ObjectId) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE mirror SET pushed = ?1 WHERE id = 1 AND remote = ?2",
            params![commit.to_hex(), remote],
        )?;
        Ok(())
    }
}
