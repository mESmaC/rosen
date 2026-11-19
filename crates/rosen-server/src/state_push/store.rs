//! What the state feed has delivered to each endpoint, kept in the metadata
//! store so a restarted server resumes from the event log instead of
//! rereading it from the start.
//!
//! The cursor has two numbers per endpoint, like the ledger push's.
//! `scanned` is the event sequence through which every document an event
//! made dirty has been acknowledged, or found unchanged. `delivered` is the
//! highest sequence the endpoint acknowledged holding. An endpoint that
//! later reports less than `delivered` lost documents, and the scan goes
//! back to what it reports.
//!
//! `state_push_sent` keeps a fingerprint of each document last delivered,
//! so a document an event marked dirty that has not changed (another
//! change's claim, a review comment on an open change for coverage) is not
//! sent again, and a closed change is sent once. `related` lists the changes
//! a change document named as overlapping, so they are rebuilt when the
//! overlap ends.

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS state_push (
    endpoint  TEXT    NOT NULL PRIMARY KEY,
    scanned   INTEGER NOT NULL,
    delivered INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS state_push_sent (
    endpoint    TEXT NOT NULL,
    document    TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    related     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (endpoint, document)
) WITHOUT ROWID;
";

/// Progress at one endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub scanned: u64,
    pub delivered: u64,
}

impl Cursor {
    /// Takes the endpoint's account of the highest sequence it holds. Less
    /// than was delivered means it lost documents, so the scan resumes
    /// from there.
    pub fn reconcile(&mut self, held: u64) {
        if held < self.delivered {
            self.scanned = self.scanned.min(held);
            self.delivered = held;
        }
    }
}

/// A delivered document's fingerprint and the changes it named.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sent {
    pub fingerprint: String,
    pub related: BTreeSet<String>,
}

pub struct StateStore {
    connection: Connection,
}

impl StateStore {
    /// Opens the feed's tables in the metadata database at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = crate::metadata::open(path)?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self { connection })
    }

    /// Progress at `endpoint`, or `None` for an endpoint never reached.
    pub fn cursor(&self, endpoint: &str) -> rusqlite::Result<Option<Cursor>> {
        self.connection
            .query_row(
                "SELECT scanned, delivered FROM state_push WHERE endpoint = ?1",
                [endpoint],
                |row| {
                    Ok(Cursor {
                        scanned: to_seq(row.get(0)?),
                        delivered: to_seq(row.get(1)?),
                    })
                },
            )
            .optional()
    }

    pub fn save_cursor(&self, endpoint: &str, cursor: Cursor) -> rusqlite::Result<()> {
        self.connection.execute(
            "INSERT INTO state_push (endpoint, scanned, delivered) VALUES (?1, ?2, ?3)
             ON CONFLICT (endpoint) DO UPDATE SET scanned = ?2, delivered = ?3",
            params![
                endpoint,
                from_seq(cursor.scanned),
                from_seq(cursor.delivered)
            ],
        )?;
        Ok(())
    }

    /// What was last delivered for `document`, if anything.
    pub fn sent(&self, endpoint: &str, document: &str) -> rusqlite::Result<Option<Sent>> {
        self.connection
            .query_row(
                "SELECT fingerprint, related FROM state_push_sent
                 WHERE endpoint = ?1 AND document = ?2",
                params![endpoint, document],
                |row| {
                    let related: String = row.get(1)?;
                    Ok(Sent {
                        fingerprint: row.get(0)?,
                        related: related.split_whitespace().map(str::to_owned).collect(),
                    })
                },
            )
            .optional()
    }

    /// Every document delivered under `prefix`, with its fingerprint.
    pub fn documents(
        &self,
        endpoint: &str,
        prefix: &str,
    ) -> rusqlite::Result<Vec<(String, String)>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT document, fingerprint FROM state_push_sent
             WHERE endpoint = ?1 AND substr(document, 1, length(?2)) = ?2",
        )?;
        let rows = statement.query_map(params![endpoint, prefix], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    /// Records delivered documents and the cursor in one transaction, so a
    /// crash between them cannot skip a document.
    pub fn delivered(
        &mut self,
        endpoint: &str,
        documents: &[(String, Sent)],
        cursor: Cursor,
    ) -> rusqlite::Result<()> {
        let tx = self.connection.transaction()?;
        for (document, sent) in documents {
            let related: Vec<&str> = sent.related.iter().map(String::as_str).collect();
            tx.execute(
                "INSERT INTO state_push_sent (endpoint, document, fingerprint, related)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (endpoint, document)
                 DO UPDATE SET fingerprint = ?3, related = ?4",
                params![endpoint, document, sent.fingerprint, related.join(" ")],
            )?;
        }
        tx.execute(
            "INSERT INTO state_push (endpoint, scanned, delivered) VALUES (?1, ?2, ?3)
             ON CONFLICT (endpoint) DO UPDATE SET scanned = ?2, delivered = ?3",
            params![
                endpoint,
                from_seq(cursor.scanned),
                from_seq(cursor.delivered)
            ],
        )?;
        tx.commit()
    }
}

fn to_seq(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn from_seq(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
