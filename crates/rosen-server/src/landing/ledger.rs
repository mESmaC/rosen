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

use super::{ChangeId, CommitId, LandError, LandResult, ObjectId, Receipt, Tier};

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
    change_id      TEXT,
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

pub(crate) fn ensure_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(SCHEMA)?;
    super::receipt_store::ensure_schema(connection)?;
    super::revert_store::migrate_history(connection)
}

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
    /// The change's head or state moved after the land read it, so the
    /// receipt and approval it decided on may no longer be for its head.
    ChangeMoved,
    /// The receipt the land decided on no longer verifies, because its
    /// machine was revoked or its row changed after the land read it.
    ReceiptUntrusted,
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

pub struct Ledger {
    connection: Mutex<Connection>,
}

impl Ledger {
    pub fn open(path: &Path) -> LandResult<Ledger> {
        Self::prepare(crate::metadata::open(path)?)
    }

    pub fn open_in_memory() -> LandResult<Ledger> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(connection: Connection) -> LandResult<Ledger> {
        // WAL lets readers proceed during a land; FULL sync makes a committed
        // land survive power loss.
        crate::metadata::configure(&connection)?;
        ensure_schema(&connection)?;
        // The land transaction reads the conflict gate, so its table must be
        // in this same database.
        conflicts::ensure_schema(&connection)?;
        Ok(Ledger {
            connection: Mutex::new(connection),
        })
    }

    pub(super) fn lock(&self) -> LandResult<MutexGuard<'_, Connection>> {
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

    /// The project's one base ref (SPEC.md §2), as name and commit, if
    /// import has created it.
    pub fn project_base(&self) -> LandResult<Option<(String, CommitId)>> {
        let connection = self.lock()?;
        let row: Option<(String, String)> = connection
            .query_row("SELECT name, target FROM base_refs LIMIT 1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        row.map(|(name, target)| Ok((name, parse_commit(target)?)))
            .transpose()
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

    /// Read the complete interval ending at the attempt's base. Never include
    /// a concurrent future land; CAS will retry against that base separately.
    pub fn lands_between(
        &self,
        ref_name: &str,
        after: i64,
        through: &CommitId,
    ) -> LandResult<Vec<(CommitId, Tier)>> {
        let connection = self.lock()?;
        let end: i64 = connection.query_row(
            "SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2",
            params![ref_name, through.to_string()],
            |r| r.get(0),
        )?;
        if end < after {
            return Err(LandError::Store(
                "receipt base is ahead of attempted base".into(),
            ));
        }
        let mut query = connection.prepare(
            "SELECT commit_id,tier FROM base_history WHERE ref_name=?1 AND seq>?2 AND seq<=?3 ORDER BY seq")?;
        let rows = query.query_map(params![ref_name, after, end], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (commit, tier) = row?;
            let tier =
                Tier::parse(&tier).ok_or_else(|| LandError::Store("invalid landed tier".into()))?;
            Ok((parse_commit(commit)?, tier))
        })
        .collect()
    }

    pub fn landing_of(&self, change: &ChangeId) -> LandResult<Option<CommitId>> {
        let connection = self.lock()?;
        let commit = connection
            .query_row(
                "SELECT commit_id FROM base_history WHERE change_id = ?1 AND NOT EXISTS (SELECT 1 FROM auto_reverts WHERE culprit=base_history.commit_id)",
                params![change.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        commit.map(parse_commit).transpose()
    }

    /// Moves the base from `expected` to `record.commit` and records the land,
    /// atomically. Refuses if the base moved or the change already landed.
    pub fn compare_and_swap(
        &self,
        expected: &CommitId,
        record: &LandRecord,
    ) -> LandResult<CasResult> {
        self.compare_and_swap_with(expected, record, None)
    }

    /// [`Ledger::compare_and_swap`] for a land decided on `receipt`, which
    /// must still verify inside the swap's transaction: a machine revoked
    /// after the land read its receipt then stops the base from moving.
    pub fn compare_and_swap_with(
        &self,
        expected: &CommitId,
        record: &LandRecord,
        receipt: Option<&Receipt>,
    ) -> LandResult<CasResult> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(result) = check_attempt(&tx, expected, record)? {
            return Ok(result);
        }
        if let Some(receipt) = receipt {
            if !super::receipt_store::stored_receipt_verifies(&tx, receipt)? {
                return Ok(CasResult::ReceiptUntrusted);
            }
        }
        // The predicate is also on the write itself, rather than relying on
        // the earlier read to be the compare half of compare-and-swap.
        let changed = tx.execute(
            "UPDATE base_refs SET target = ?1 WHERE name = ?2 AND target = ?3",
            params![
                record.commit.to_string(),
                record.ref_name,
                expected.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(LandError::Store(
                "base CAS did not update exactly one row".into(),
            ));
        }
        let seq = record_land(&tx, record)?;
        close_registered_change(&tx, record)?;
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

    /// True while the land that made `commit` has unfinished post-land
    /// work. Keyed by the landed commit, not the change: a reverted change
    /// can land again (R12), and each land finishes on its own.
    pub fn post_land_pending(&self, ref_name: &str, commit: &CommitId) -> LandResult<bool> {
        let connection = self.lock()?;
        let pending = connection
            .query_row(
                "SELECT post_land_done = 0 FROM base_history
                 WHERE ref_name = ?1 AND commit_id = ?2",
                params![ref_name, commit.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(pending.unwrap_or(false))
    }

    pub fn mark_post_land_done(&self, ref_name: &str, commit: &CommitId) -> LandResult<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE base_history SET post_land_done = 1 WHERE ref_name = ?1 AND commit_id = ?2",
            params![ref_name, commit.to_string()],
        )?;
        Ok(())
    }
}

/// Evaluate mutable gates under the same SQLite writer reservation as the
/// ref move. Returning a refusal drops the transaction without any writes.
fn check_attempt(
    tx: &Connection,
    expected: &CommitId,
    record: &LandRecord,
) -> LandResult<Option<CasResult>> {
    let landed: Option<String> = tx
        .query_row(
            "SELECT commit_id FROM base_history WHERE change_id = ?1 AND NOT EXISTS (SELECT 1 FROM auto_reverts WHERE culprit=base_history.commit_id)",
            params![record.change.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(commit) = landed {
        return Ok(Some(CasResult::AlreadyLanded {
            commit: parse_commit(commit)?,
        }));
    }
    if !change_unmoved(tx, record)? {
        return Ok(Some(CasResult::ChangeMoved));
    }
    match conflicts::check_landable(tx, &record.change.to_string()) {
        Ok(()) => {}
        Err(ConflictStoreError::Conflict(_)) => return Ok(Some(CasResult::OpenConflicts)),
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
    (current != expected.to_string())
        .then(|| parse_commit(current).map(|current| CasResult::BaseMoved { current }))
        .transpose()
}

/// Persist history before closing the registry, so any error in either
/// operation rolls the ref, history and state back together.
fn record_land(tx: &Connection, record: &LandRecord) -> LandResult<i64> {
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
    Ok(tx.last_insert_rowid())
}

fn has_registry(tx: &Connection) -> LandResult<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'changes'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Use the registry's core state rules and persistence, inside this land's
/// transaction. Otherwise a snapshot could be acknowledged after its head
/// landed but before post-land recovery closed the change.
fn close_registered_change(tx: &Connection, record: &LandRecord) -> LandResult<()> {
    if !has_registry(tx)? {
        return Ok(());
    }
    crate::changes::update_in(tx, record.change, |change| {
        if change.state == rosen_core::ChangeState::Open {
            change.mark_ready()?;
        }
        change.mark_landed()
    })
    .map_err(|error| LandError::Store(error.to_string()))?;
    Ok(())
}

/// True while the change registry still holds the change at the head the
/// land merged, in a state that may land, with an approval that still holds
/// (R7). Read inside the land transaction: a snapshot stored between the
/// land's read and its swap would otherwise be marked landed without ever
/// reaching the base, and one that left the head where it was read would
/// carry the approval it voided onto the base.
/// A ledger used without the change registry has nothing to contradict.
fn change_unmoved(tx: &Connection, record: &LandRecord) -> LandResult<bool> {
    if !has_registry(tx)? {
        return Ok(true);
    }
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT state, head FROM changes WHERE id = ?1",
            params![record.change.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let unmoved = row.is_some_and(|(state, head)| {
        matches!(state.as_str(), "open" | "ready") && head == record.head.to_hex()
    });
    Ok(unmoved && approval_holds(tx, &record.change)?)
}

/// An author snapshot sets every approval it voids to invalid for good, so
/// a valid row means an approval covers the change's current content even
/// if the head left and came back. A registry without the review table
/// holds no approvals, so it refuses.
fn approval_holds(tx: &Connection, change: &ChangeId) -> LandResult<bool> {
    let has_reviews = tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'review_approvals'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_reviews {
        return Ok(false);
    }
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM review_approvals WHERE change_id = ?1 AND valid = 1)",
        params![change.to_string()],
        |row| row.get(0),
    )?)
}

/// Position of `commit` in the ref's history. The commit must be one the
/// ref has held: every base a land attempt reads was recorded there first.
pub(super) fn history_seq(
    connection: &Connection,
    ref_name: &str,
    commit: &CommitId,
) -> LandResult<i64> {
    connection
        .query_row(
            "SELECT seq FROM base_history WHERE ref_name = ?1 AND commit_id = ?2",
            params![ref_name, commit.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| LandError::Store(format!("{commit} is not in the history of {ref_name}")))
}

/// Reads a commit ID the ledger stored as hex.
pub(super) fn parse_commit(hex: String) -> LandResult<CommitId> {
    Ok(ObjectId::from_hex(&hex)?)
}
