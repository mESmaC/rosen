//! Signed receipts in the landing ledger (SPEC.md §2 "Receipt", R8, R14).
//!
//! A receipt is stored with the machine that signed it and its signature
//! over [`Receipt::signing_payload`]. The signature is checked against the
//! machine's bound key when the receipt is stored and again whenever a land
//! decides on it, so a row that no submit verified, a receipt whose fields
//! no longer match what was signed, or a receipt from a machine revoked
//! since never moves the base.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::ledger::{history_seq, parse_commit};
use super::{AgentId, ChangeId, CommitId, LandError, LandResult, Ledger, Receipt};
use crate::machines::signing::{self, SERVER_MACHINE};

/// A receipt with the machine that signed it and its signature (hex).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedReceipt {
    pub receipt: Receipt,
    pub machine: String,
    pub signature: String,
}

/// The best stored receipt for a change's current head and check set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptLookup {
    Missing,
    /// Receipts exist for the change, but none for its head and check set
    /// that a trusted machine's key verifies.
    Invalid,
    /// `base_seq` is the receipt base's position in the base history.
    Valid {
        receipt: Receipt,
        base_seq: i64,
    },
    /// The newest receipt is bound to a base the ref reached after the
    /// attempt read it: the base moved during the attempt, so the attempt
    /// must be retried on the new base rather than decided on the old one.
    Ahead,
}

/// What binding a machine's signing key found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyBinding {
    /// The machine signs with this key, newly bound or bound before.
    Bound,
    /// The machine is bound to a different key.
    Conflict,
}

pub(super) fn ensure_schema(connection: &Connection) -> rusqlite::Result<()> {
    // Receipts stored before signing have no signer, so they never verify
    // and a land asks for a new one, rather than trusting an unsigned row.
    for column in ["machine", "signature"] {
        let present: bool = connection.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('receipts') WHERE name = ?1",
            [column],
            |row| row.get(0),
        )?;
        if !present {
            connection.execute_batch(&format!(
                "ALTER TABLE receipts ADD COLUMN {column} TEXT NOT NULL DEFAULT ''"
            ))?;
        }
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS machine_signing_keys (
            machine    TEXT PRIMARY KEY,
            public_key TEXT NOT NULL
        );",
    )?;
    // Verification reads machine status, so its table must be here too.
    crate::machines::ensure_schema(connection)
}

impl Ledger {
    /// Binds `machine` to `public_key` the first time it signs, and reports
    /// whether it signs with this key. A bound key is never replaced, so a
    /// stolen credential cannot swap in a key of its own.
    pub fn bind_signing_key(&self, machine: &str, public_key: &str) -> LandResult<KeyBinding> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT OR IGNORE INTO machine_signing_keys (machine, public_key) VALUES (?1, ?2)",
            params![machine, public_key],
        )?;
        let bound: String = connection.query_row(
            "SELECT public_key FROM machine_signing_keys WHERE machine = ?1",
            params![machine],
            |row| row.get(0),
        )?;
        Ok(if bound == public_key {
            KeyBinding::Bound
        } else {
            KeyBinding::Conflict
        })
    }

    /// Records the server's own machine key. Unlike a remote machine's, it
    /// is replaced if the key file was lost: receipts it signed stop
    /// verifying and are submitted again, which fails safe.
    pub fn set_server_key(&self, public_key: &str) -> LandResult<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT OR REPLACE INTO machine_signing_keys (machine, public_key) VALUES (?1, ?2)",
            params![SERVER_MACHINE, public_key],
        )?;
        Ok(())
    }

    /// Stores a signed receipt and returns the stored one. Refuses a
    /// signature the machine's bound key does not verify. Submitting the
    /// same `(change, head, base, check set)` again keeps the first
    /// submitter, unless that stored receipt no longer verifies: then the
    /// new one replaces it, so a revoked machine's receipt can be redone.
    pub fn put_receipt(&self, signed: &SignedReceipt) -> LandResult<Receipt> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !verifies(&tx, &signed.receipt, &signed.machine, &signed.signature)? {
            return Err(LandError::InvalidReceipt(format!(
                "the signature does not verify with machine {}'s receipt key",
                signed.machine
            )));
        }
        let receipt = &signed.receipt;
        let key = [
            receipt.change.to_string(),
            receipt.head.to_string(),
            receipt.base.to_string(),
            receipt.check_set.clone(),
        ];
        let stored: Option<(String, String, String)> = tx
            .query_row(
                "SELECT submitter, machine, signature FROM receipts
                 WHERE change_id = ?1 AND head = ?2 AND base = ?3 AND check_set = ?4",
                params![key[0], key[1], key[2], key[3]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((submitter, machine, signature)) = stored {
            let first = Receipt {
                submitter: AgentId::new(submitter)?,
                ..receipt.clone()
            };
            if verifies(&tx, &first, &machine, &signature)? {
                return Ok(first);
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO receipts
                 (change_id, head, base, check_set, submitter, machine, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                key[0],
                key[1],
                key[2],
                key[3],
                receipt.submitter.as_str(),
                signed.machine,
                signed.signature
            ],
        )?;
        tx.commit()?;
        Ok(receipt.clone())
    }

    /// Finds the verified receipt for `head` and `check_set` whose base is
    /// newest in the ref's history, which leaves the fewest lands to
    /// re-check. `attempt_base` is the base the land attempt read; a receipt
    /// bound to a later base is reported as [`ReceiptLookup::Ahead`].
    pub fn best_receipt(
        &self,
        ref_name: &str,
        change: &ChangeId,
        head: &CommitId,
        check_set: &str,
        attempt_base: &CommitId,
    ) -> LandResult<ReceiptLookup> {
        let connection = self.lock()?;
        let candidates = candidates(&connection, ref_name, change, head, check_set)?;
        for (base, submitter, machine, signature, base_seq) in candidates {
            let receipt = Receipt {
                change: *change,
                head: *head,
                base: parse_commit(base)?,
                check_set: check_set.to_owned(),
                submitter: AgentId::new(submitter)?,
            };
            if !verifies(&connection, &receipt, &machine, &signature)? {
                continue;
            }
            if base_seq > history_seq(&connection, ref_name, attempt_base)? {
                return Ok(ReceiptLookup::Ahead);
            }
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
}

/// True when the stored row for `receipt`'s binding still verifies, read
/// on the caller's connection so it can run inside the land transaction.
pub(super) fn stored_receipt_verifies(
    connection: &Connection,
    receipt: &Receipt,
) -> LandResult<bool> {
    let stored: Option<(String, String, String)> = connection
        .query_row(
            "SELECT submitter, machine, signature FROM receipts
             WHERE change_id = ?1 AND head = ?2 AND base = ?3 AND check_set = ?4",
            params![
                receipt.change.to_string(),
                receipt.head.to_string(),
                receipt.base.to_string(),
                receipt.check_set
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((submitter, machine, signature)) = stored else {
        return Ok(false);
    };
    let unchanged = submitter == receipt.submitter.as_str();
    Ok(unchanged && verifies(connection, receipt, &machine, &signature)?)
}

type Candidate = (String, String, String, String, i64);

/// Receipts for the change's head and check set on a base the ref held,
/// newest base first.
fn candidates(
    connection: &Connection,
    ref_name: &str,
    change: &ChangeId,
    head: &CommitId,
    check_set: &str,
) -> LandResult<Vec<Candidate>> {
    let mut query = connection.prepare(
        "SELECT r.base, r.submitter, r.machine, r.signature, h.seq FROM receipts r
         JOIN base_history h ON h.ref_name = ?1 AND h.commit_id = r.base
         WHERE r.change_id = ?2 AND r.head = ?3 AND r.check_set = ?4
         ORDER BY h.seq DESC",
    )?;
    let rows = query.query_map(
        params![ref_name, change.to_string(), head.to_string(), check_set],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// True when `machine` is trusted now (the server itself, or an enrolled
/// machine that is not revoked) and its bound key verifies `signature`
/// over the receipt.
fn verifies(
    connection: &Connection,
    receipt: &Receipt,
    machine: &str,
    signature: &str,
) -> LandResult<bool> {
    let key: Option<String> = connection
        .query_row(
            "SELECT k.public_key FROM machine_signing_keys k
             LEFT JOIN machines m ON m.id = k.machine
             WHERE k.machine = ?1 AND (k.machine = ?2 OR m.status = 'active')",
            params![machine, SERVER_MACHINE],
            |row| row.get(0),
        )
        .optional()?;
    Ok(key.is_some_and(|key| signing::verify(&key, &receipt.signing_payload(), signature)))
}
