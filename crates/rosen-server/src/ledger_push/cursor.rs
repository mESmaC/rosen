//! How far each ledger has reached the endpoint, kept in the metadata store
//! so a restarted server resumes where it stopped instead of resending
//! every entry.
//!
//! Each ledger has two numbers per endpoint. `scanned` is the sequence up to
//! which every entry the push sends has been acknowledged; entries it skips
//! (Velocity's versioned records) move it too, so they are read once.
//! `delivered` is the highest sequence the endpoint acknowledged. When the
//! endpoint later reports holding less than `delivered`, it lost entries,
//! and `scanned` goes back to what it reports so they are sent again.

use std::path::Path;

use rosen_protocol::channel::Ledger;
use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ledger_push (
    endpoint  TEXT    NOT NULL,
    ledger    TEXT    NOT NULL CHECK (ledger IN ('discovery', 'past_work')),
    scanned   INTEGER NOT NULL,
    delivered INTEGER NOT NULL,
    PRIMARY KEY (endpoint, ledger)
) WITHOUT ROWID;
";

/// One value for each of the two ledgers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerLedger<T> {
    pub discovery: T,
    pub past_work: T,
}

impl<T> PerLedger<T> {
    pub fn get(&self, ledger: Ledger) -> &T {
        match ledger {
            Ledger::Discovery => &self.discovery,
            Ledger::PastWork => &self.past_work,
        }
    }

    pub fn get_mut(&mut self, ledger: Ledger) -> &mut T {
        match ledger {
            Ledger::Discovery => &mut self.discovery,
            Ledger::PastWork => &mut self.past_work,
        }
    }
}

pub const LEDGERS: [Ledger; 2] = [Ledger::Discovery, Ledger::PastWork];

/// One ledger's progress at one endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub scanned: u64,
    pub delivered: u64,
}

pub type Cursors = PerLedger<Cursor>;

/// The highest sequence the endpoint holds in each ledger.
pub type Held = PerLedger<u64>;

/// What one batch covered: the last sequence it read and the last it sent,
/// in each ledger. Zero means none.
pub type Extent = PerLedger<Cursor>;

impl Cursors {
    /// Takes the endpoint's own account of what it holds. Holding less than
    /// was delivered means it lost entries, so the push resends from there.
    pub fn reconcile(&mut self, held: &Held) {
        for ledger in LEDGERS {
            let cursor = self.get_mut(ledger);
            let held = *held.get(ledger);
            if held < cursor.delivered {
                cursor.scanned = cursor.scanned.min(held);
                cursor.delivered = held;
            }
        }
    }

    /// Moves past rows the push read and skipped, sending nothing.
    pub fn skip(&mut self, batch: &Extent) -> bool {
        for ledger in LEDGERS {
            let cursor = self.get_mut(ledger);
            cursor.scanned = cursor.scanned.max(batch.get(ledger).scanned);
        }
        true
    }

    /// Advances past a batch the endpoint accepted. Returns false, and goes
    /// back to what the endpoint holds, when it reports less than the batch
    /// sent: the entries were not kept, so they must be sent again.
    pub fn acknowledge(&mut self, batch: &Extent, held: &Held) -> bool {
        let kept = LEDGERS
            .iter()
            .all(|&ledger| batch.get(ledger).delivered <= *held.get(ledger));
        if kept {
            for ledger in LEDGERS {
                let cursor = self.get_mut(ledger);
                let covered = batch.get(ledger);
                cursor.scanned = cursor.scanned.max(covered.scanned);
                cursor.delivered = cursor.delivered.max(covered.delivered);
            }
        }
        self.reconcile(held);
        kept
    }
}

pub struct CursorStore {
    connection: Connection,
}

impl CursorStore {
    /// Opens the ledger push table in the metadata database at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = crate::metadata::open(path)?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self { connection })
    }

    /// The metadata connection, for reading the ledgers themselves.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Progress at `endpoint`; a new endpoint starts from the beginning.
    pub fn load(&self, endpoint: &str) -> rusqlite::Result<Cursors> {
        let mut cursors = Cursors::default();
        for ledger in LEDGERS {
            let row = self
                .connection
                .query_row(
                    "SELECT scanned, delivered FROM ledger_push
                     WHERE endpoint = ?1 AND ledger = ?2",
                    params![endpoint, ledger.as_str()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            if let Some((scanned, delivered)) = row {
                *cursors.get_mut(ledger) = Cursor {
                    scanned: u64::try_from(scanned).unwrap_or(0),
                    delivered: u64::try_from(delivered).unwrap_or(0),
                };
            }
        }
        Ok(cursors)
    }

    /// Stores both ledgers' progress at `endpoint` in one transaction.
    pub fn save(&mut self, endpoint: &str, cursors: &Cursors) -> rusqlite::Result<()> {
        let tx = self.connection.transaction()?;
        for ledger in LEDGERS {
            let cursor = cursors.get(ledger);
            tx.execute(
                "INSERT INTO ledger_push (endpoint, ledger, scanned, delivered)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (endpoint, ledger)
                 DO UPDATE SET scanned = ?3, delivered = ?4",
                params![
                    endpoint,
                    ledger.as_str(),
                    i64::try_from(cursor.scanned).unwrap_or(i64::MAX),
                    i64::try_from(cursor.delivered).unwrap_or(i64::MAX),
                ],
            )?;
        }
        tx.commit()
    }
}
