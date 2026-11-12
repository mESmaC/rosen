//! The landing tables of the SQLite metadata store (SPEC D13, §3).
//!
//! The base ref, the history of every commit it has held, and receipts live
//! here. A land is one `BEGIN IMMEDIATE` transaction that checks the ref
//! still holds the expected commit, records the landing and moves the ref, so
//! a crash leaves the base at either the old or the new commit (R8).

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::conflicts::{self, ConflictStoreError};

use super::{AgentId, ChangeId, CommitId, LandError, LandResult, ObjectId, Receipt, Tier};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS base_refs (
    name   TEXT PRIMARY KEY,
    target TEXT NOT NULL
);
-- Every commit a base ref has held, in order. The first row of a ref is its
-- initial commit (change_id NULL); every later row is one land.
CREATE TABLE IF NOT EXISTS base_history (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    ref_name       TEXT NOT NULL,
    commit_id      TEXT NOT NULL,
    change_id      TEXT UNIQUE,
    head           TEXT,
    tier           TEXT NOT NULL,
    post_land_done INTEGER NOT NULL DEFAULT 0,
    UNIQUE (ref_name, commit_id)
);
CREATE TABLE IF NOT EXISTS receipts (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    change_id TEXT NOT NULL,
    head      TEXT NOT NULL,
    base      TEXT NOT NULL,
    check_set TEXT NOT NULL,
    submitter TEXT NOT NULL,
    UNIQUE (change_id, head, base, check_set)
);
";

/// Tier recorded for a ref's initial commit; never counts as prose-only.
const INITIAL_TIER: &str = "initial";

/// One land, as recorded in the base history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandRecord {
    pub ref_name: String,
    pub change: ChangeId,
    pub head: CommitId,
    pub commit: CommitId,
    pub tier: Tier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CasResult {
    Swapped {
        seq: i64,
    },
    /// Another land moved the base first.
    BaseMoved {
        current: CommitId,
    },
    /// The change already landed: a land is never applied twice.
    AlreadyLanded {
        commit: CommitId,
    },
    /// The change gained an open conflict after the land read it.
    OpenConflicts,
}

/// What [`Ledger::init_base`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitBase {
    Created,
    /// The project already has a base, under this name and at this commit.
    Exists {
        name: String,
        commit: CommitId,
    },
}

/// The best stored receipt for a change's current head and check set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptLookup {
    Missing,
    Invalid,
    /// `base_seq` is the receipt base's position in the base history.
    Valid {
        receipt: Receipt,
        base_seq: i64,
    },
}

pub struct Ledger {
    connection: Mutex<Connection>,
}

impl Ledger {
    pub fn open(path: &Path) -> LandResult<Ledger> {
        Self::prepare(Connection::open(path)?)
    }

    pub fn open_in_memory() -> LandResult<Ledger> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(connection: Connection) -> LandResult<Ledger> {
        // WAL lets readers proceed during a land; FULL sync makes a committed
        // land survive power loss.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        // The land transaction reads the conflict gate, so its table must be
        // in this same database.
        conflicts::ensure_schema(&connection)?;
        Ok(Ledger {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> LandResult<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| LandError::Store("ledger mutex poisoned".into()))
    }

    /// Creates the project's base ref at its first commit (import, R1).
    /// A project has exactly one base (SPEC.md §2), so this refuses, changing
    /// nothing, when any base ref already exists, and names that base.
    pub fn init_base(&self, ref_name: &str, commit: &CommitId) -> LandResult<InitBase> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row("SELECT name, target FROM base_refs LIMIT 1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        if let Some((name, target)) = existing {
            let commit = parse_commit(target)?;
            return Ok(InitBase::Exists { name, commit });
        }
        tx.execute(
            "INSERT INTO base_refs (name, target) VALUES (?1, ?2)",
            params![ref_name, commit.to_string()],
        )?;
        tx.execute(
            "INSERT INTO base_history (ref_name, commit_id, tier) VALUES (?1, ?2, ?3)",
            params![ref_name, commit.to_string(), INITIAL_TIER],
        )?;
        tx.commit()?;
        Ok(InitBase::Created)
    }

    pub fn base(&self, ref_name: &str) -> LandResult<Option<CommitId>> {
        let connection = self.lock()?;
        let target = connection
            .query_row(
                "SELECT target FROM base_refs WHERE name = ?1",
                params![ref_name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        target.map(parse_commit).transpose()
    }

    /// Position of `commit` in the ref's history, if the ref ever held it.
    pub fn base_seq(&self, ref_name: &str, commit: &CommitId) -> LandResult<Option<i64>> {
        let connection = self.lock()?;
        let seq = connection
            .query_row(
                "SELECT seq FROM base_history WHERE ref_name = ?1 AND commit_id = ?2",
                params![ref_name, commit.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(seq)
    }

    /// True when every land after history position `seq` is prose-only.
    pub fn landed_since_all_prose(&self, ref_name: &str, seq: i64) -> LandResult<bool> {
        let connection = self.lock()?;
        let non_prose: i64 = connection.query_row(
            "SELECT COUNT(*) FROM base_history
             WHERE ref_name = ?1 AND seq > ?2 AND tier != ?3",
            params![ref_name, seq, Tier::ProseOnly.as_str()],
            |row| row.get(0),
        )?;
        Ok(non_prose == 0)
    }

    pub fn landing_of(&self, change: &ChangeId) -> LandResult<Option<CommitId>> {
        let connection = self.lock()?;
        let commit = connection
            .query_row(
                "SELECT commit_id FROM base_history WHERE change_id = ?1",
                params![change.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        commit.map(parse_commit).transpose()
    }

    /// Stores a receipt. Submitting the same receipt twice is a no-op.
    pub fn put_receipt(&self, receipt: &Receipt) -> LandResult<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT OR IGNORE INTO receipts (change_id, head, base, check_set, submitter)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                receipt.change.to_string(),
                receipt.head.to_string(),
                receipt.base.to_string(),
                receipt.check_set,
                receipt.submitter.as_str()
            ],
        )?;
        Ok(())
    }

    /// Finds the receipt for `head` and `check_set` whose base is newest in
    /// the ref's history, which leaves the fewest lands to re-check.
    pub fn best_receipt(
        &self,
        ref_name: &str,
        change: &ChangeId,
        head: &CommitId,
        check_set: &str,
    ) -> LandResult<ReceiptLookup> {
        let connection = self.lock()?;
        let valid = connection
            .query_row(
                "SELECT r.base, r.submitter, h.seq FROM receipts r
                 JOIN base_history h ON h.ref_name = ?1 AND h.commit_id = r.base
                 WHERE r.change_id = ?2 AND r.head = ?3 AND r.check_set = ?4
                 ORDER BY h.seq DESC LIMIT 1",
                params![ref_name, change.to_string(), head.to_string(), check_set],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((base, submitter, base_seq)) = valid {
            let receipt = Receipt {
                change: *change,
                head: *head,
                base: parse_commit(base)?,
                check_set: check_set.to_owned(),
                submitter: AgentId::new(submitter)?,
            };
            return Ok(ReceiptLookup::Valid { receipt, base_seq });
        }
        let any: i64 = connection.query_row(
            "SELECT COUNT(*) FROM receipts WHERE change_id = ?1",
            params![change.to_string()],
            |row| row.get(0),
        )?;
        Ok(if any == 0 {
            ReceiptLookup::Missing
        } else {
            ReceiptLookup::Invalid
        })
    }

    /// Moves the base from `expected` to `record.commit` and records the land,
    /// atomically. Refuses if the base moved or the change already landed.
    pub fn compare_and_swap(
        &self,
        expected: &CommitId,
        record: &LandRecord,
    ) -> LandResult<CasResult> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let landed: Option<String> = tx
            .query_row(
                "SELECT commit_id FROM base_history WHERE change_id = ?1",
                params![record.change.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(commit) = landed {
            return Ok(CasResult::AlreadyLanded {
                commit: parse_commit(commit)?,
            });
        }
        match conflicts::check_landable(&tx, &record.change.to_string()) {
            Ok(()) => {}
            Err(ConflictStoreError::Conflict(_)) => return Ok(CasResult::OpenConflicts),
            Err(ConflictStoreError::Sql(error)) => return Err(error.into()),
        }
        let current: String = tx
            .query_row(
                "SELECT target FROM base_refs WHERE name = ?1",
                params![record.ref_name],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| LandError::NoBase(record.ref_name.clone()))?;
        if current != expected.to_string() {
            return Ok(CasResult::BaseMoved {
                current: parse_commit(current)?,
            });
        }
        tx.execute(
            "INSERT INTO base_history (ref_name, commit_id, change_id, head, tier)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.ref_name,
                record.commit.to_string(),
                record.change.to_string(),
                record.head.to_string(),
                record.tier.as_str()
            ],
        )?;
        let seq = tx.last_insert_rowid();
        tx.execute(
            "UPDATE base_refs SET target = ?1 WHERE name = ?2",
            params![record.commit.to_string(), record.ref_name],
        )?;
        tx.commit()?;
        Ok(CasResult::Swapped { seq })
    }

    /// Lands whose post-land work (R8 step 4) has not finished, oldest first.
    /// After a crash the server replays them; post-land work is idempotent.
    pub fn pending_post_land(&self, ref_name: &str) -> LandResult<Vec<LandRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT change_id, head, commit_id, tier FROM base_history
             WHERE ref_name = ?1 AND change_id IS NOT NULL AND post_land_done = 0
             ORDER BY seq",
        )?;
        let rows = statement.query_map(params![ref_name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (change, head, commit, tier) = row?;
            let tier = Tier::parse(&tier)
                .ok_or_else(|| LandError::Store(format!("unknown tier {tier}")))?;
            records.push(LandRecord {
                ref_name: ref_name.to_owned(),
                change: change.parse()?,
                head: parse_commit(head)?,
                commit: parse_commit(commit)?,
                tier,
            });
        }
        Ok(records)
    }

    pub fn mark_post_land_done(&self, change: &ChangeId) -> LandResult<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE base_history SET post_land_done = 1 WHERE change_id = ?1",
            params![change.to_string()],
        )?;
        Ok(())
    }
}

/// Reads a commit ID the ledger stored as hex.
fn parse_commit(hex: String) -> LandResult<CommitId> {
    Ok(ObjectId::from_hex(&hex)?)
}
