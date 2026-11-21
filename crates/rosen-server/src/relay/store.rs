//! What the action relay keeps in the metadata store: the operator's
//! allow-list, how far it has read each endpoint's doorbell and queue, and
//! every action it took up with its outcome.
//!
//! An action's row is written as `running` before it runs and gets its
//! outcome right after, so a server stopped in between finds it at the next
//! start and never takes it up as new. The outcome stays until Collapse
//! accepted it, so a stop before the result post re-posts the same result.

use std::path::Path;

use rosen_protocol::{RelayKind, RelaySetting, COLLAPSE_MACHINE};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS relay_setting (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    allow      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS relay_cursor (
    endpoint   TEXT    NOT NULL PRIMARY KEY,
    generation INTEGER,
    after      INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS relay_actions (
    endpoint   TEXT    NOT NULL,
    id         INTEGER NOT NULL,
    action     TEXT    NOT NULL,
    state      TEXT    NOT NULL CHECK (state IN ('running', 'done', 'refused')),
    result     TEXT,
    posted     INTEGER NOT NULL DEFAULT 0,
    started_at INTEGER NOT NULL,
    PRIMARY KEY (endpoint, id)
) WITHOUT ROWID;
";

/// Finished actions are forgotten this long after they started, once
/// posted. Collapse hands out an action for 10 minutes and never repeats
/// one at or below the cursor, so a week is far more than a repeat needs.
const KEPT_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// An action already taken up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Taken {
    /// It started and never recorded an outcome: the server stopped.
    Running { action: String, started_at: i64 },
    /// Its outcome, and whether Collapse has accepted it.
    Finished {
        state: String,
        result: String,
        posted: bool,
    },
}

/// A finished action whose result Collapse has not accepted yet.
pub struct Unposted {
    pub id: i64,
    pub state: String,
    pub result: String,
}

/// How far one endpoint has been read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// The doorbell generation the last fetch answered, if any.
    pub generation: Option<i64>,
    /// The highest action ID taken up.
    pub after: i64,
}

pub struct RelayStore {
    conn: Connection,
}

impl RelayStore {
    /// Opens the relay's tables, and the machine tables the `collapse`
    /// machine lives in, in the metadata database under `data_dir`.
    pub fn open(data_dir: &Path) -> rusqlite::Result<Self> {
        let conn = crate::machines::open(data_dir)?;
        conn.execute_batch(SCHEMA)?;
        ensure_updated_at(&conn)?;
        Ok(Self { conn })
    }

    /// The operator's setting, and whether the `collapse` machine was
    /// revoked since it was made.
    pub fn setting(&self) -> rusqlite::Result<RelaySetting> {
        let allow: Option<String> = self
            .conn
            .query_row("SELECT allow FROM relay_setting WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        let status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM machines WHERE id = ?1",
                [COLLAPSE_MACHINE],
                |r| r.get(0),
            )
            .optional()?;
        let allow = allow
            .unwrap_or_default()
            .split(',')
            .filter_map(RelayKind::parse)
            .collect();
        Ok(RelaySetting {
            allow,
            revoked: status.as_deref() == Some("revoked"),
        })
    }

    /// Stores the allow-list. Allowing any kind records the `collapse`
    /// machine as R14 records machines, active again if it was revoked:
    /// this operator command is its enrollment. It has no credential, so
    /// no peer can ever authenticate as it.
    pub fn set(&mut self, allow: &[RelayKind], now_secs: u64) -> rusqlite::Result<RelaySetting> {
        let mut kinds = allow.to_vec();
        kinds.sort();
        kinds.dedup();
        let text: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO relay_setting (id, allow, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE
             SET allow = excluded.allow, updated_at = excluded.updated_at",
            params![text.join(","), now_secs],
        )?;
        if !kinds.is_empty() {
            tx.execute(
                "INSERT INTO machines (id, name, role, status, last_contact, credential_hash, expires_at)
                 VALUES (?1, 'Collapse action relay', 'agent', 'active', ?2, '', 0)
                 ON CONFLICT (id) DO UPDATE SET status = 'active'",
                params![COLLAPSE_MACHINE, now_secs],
            )?;
        }
        tx.commit()?;
        self.setting()
    }

    /// Records that the relay just acted for Collapse, for the machine list.
    pub fn touch_machine(&self, now_secs: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE machines SET last_contact = ?2 WHERE id = ?1 AND last_contact < ?2",
            params![COLLAPSE_MACHINE, now_secs],
        )?;
        Ok(())
    }

    pub fn cursor(&self, endpoint: &str) -> rusqlite::Result<Cursor> {
        let found = self
            .conn
            .query_row(
                "SELECT generation, after FROM relay_cursor WHERE endpoint = ?1",
                [endpoint],
                |r| {
                    Ok(Cursor {
                        generation: r.get(0)?,
                        after: r.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(found.unwrap_or_default())
    }

    pub fn save_cursor(&self, endpoint: &str, cursor: Cursor) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO relay_cursor (endpoint, generation, after) VALUES (?1, ?2, ?3)
             ON CONFLICT (endpoint) DO UPDATE
             SET generation = excluded.generation, after = MAX(after, excluded.after)",
            params![endpoint, cursor.generation, cursor.after],
        )?;
        Ok(())
    }

    /// The action `id` if it was already taken up.
    pub fn taken(&self, endpoint: &str, id: i64) -> rusqlite::Result<Option<Taken>> {
        self.conn
            .query_row(
                "SELECT action, state, result, posted, started_at FROM relay_actions
                 WHERE endpoint = ?1 AND id = ?2",
                params![endpoint, id],
                |r| {
                    let state: String = r.get(1)?;
                    Ok(match r.get::<_, Option<String>>(2)? {
                        Some(result) if state != "running" => Taken::Finished {
                            state,
                            result,
                            posted: r.get(3)?,
                        },
                        _ => Taken::Running {
                            action: r.get(0)?,
                            started_at: r.get(4)?,
                        },
                    })
                },
            )
            .optional()
    }

    /// Records that `id` is about to run. False when it was already taken
    /// up, so it never runs twice.
    pub fn start(
        &self,
        endpoint: &str,
        id: i64,
        action: &str,
        now_ms: i64,
    ) -> rusqlite::Result<bool> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO relay_actions (endpoint, id, action, state, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![endpoint, id, action, now_ms],
        )?;
        Ok(inserted == 1)
    }

    /// Stores what the action came to, before anything is posted.
    pub fn finish(
        &self,
        endpoint: &str,
        id: i64,
        state: &str,
        result: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE relay_actions SET state = ?3, result = ?4 WHERE endpoint = ?1 AND id = ?2",
            params![endpoint, id, state, result],
        )?;
        Ok(())
    }

    pub fn posted(&self, endpoint: &str, id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE relay_actions SET posted = 1 WHERE endpoint = ?1 AND id = ?2",
            params![endpoint, id],
        )?;
        Ok(())
    }

    /// Outcomes Collapse has not accepted yet, oldest first.
    pub fn unposted(&self, endpoint: &str) -> rusqlite::Result<Vec<Unposted>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, state, result FROM relay_actions
             WHERE endpoint = ?1 AND posted = 0 AND state != 'running' ORDER BY id",
        )?;
        let rows = stmt.query_map([endpoint], |r| {
            Ok(Unposted {
                id: r.get(0)?,
                state: r.get(1)?,
                result: r.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Actions a stopped server left running at `endpoint`, oldest first.
    pub fn interrupted(&self, endpoint: &str) -> rusqlite::Result<Vec<(i64, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, action, started_at FROM relay_actions
             WHERE endpoint = ?1 AND state = 'running' ORDER BY id",
        )?;
        let rows = stmt.query_map([endpoint], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Forgets posted outcomes older than a week.
    pub fn prune(&self, now_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM relay_actions WHERE posted = 1 AND started_at < ?1",
            [now_ms - KEPT_MS],
        )?;
        Ok(())
    }
}

fn ensure_updated_at(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(relay_setting)")?;
    let mut has_column = false;
    for col in stmt.query_map([], |row| row.get::<_, String>(1))? {
        if col? == "updated_at" {
            has_column = true;
        }
    }
    if !has_column {
        conn.execute_batch(
            "ALTER TABLE relay_setting ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Risk (R14, #404): Collapse repeats an action until it holds a result,
    // and a server can stop between running one and posting it. A second
    // start of the same ID, or a lost outcome, would land or approve twice.
    #[test]
    fn an_action_starts_once_and_keeps_its_outcome_until_posted() {
        let dir = tempfile::tempdir().unwrap();
        let store = RelayStore::open(dir.path()).unwrap();
        let endpoint = "https://collapse.test/o/r";
        assert!(store.start(endpoint, 7, "{}", 1).unwrap());
        assert!(!store.start(endpoint, 7, "{}", 2).unwrap());
        assert_eq!(store.interrupted(endpoint).unwrap().len(), 1);

        store.finish(endpoint, 7, "done", "{\"id\":1}").unwrap();
        // A restart reopens the store and still finds the unposted outcome.
        let reopened = RelayStore::open(dir.path()).unwrap();
        assert!(reopened.interrupted(endpoint).unwrap().is_empty());
        let unposted = reopened.unposted(endpoint).unwrap();
        assert_eq!(unposted.len(), 1);
        assert_eq!(unposted[0].result, "{\"id\":1}");

        reopened.posted(endpoint, 7).unwrap();
        assert!(reopened.unposted(endpoint).unwrap().is_empty());
        let taken = reopened.taken(endpoint, 7).unwrap();
        assert!(matches!(taken, Some(Taken::Finished { posted: true, .. })));
    }
}
