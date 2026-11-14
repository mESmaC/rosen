//! Durable R7 approvals. Invalidating an author's changed head happens in
//! the snapshot transaction; rebases preserve both validity and review history.
use super::ReviewError;
use crate::changes;
use crate::events::Staged;
use rosen_core::{ChangeId, ObjectId};
use rosen_protocol::{Approval, ApproveRequest, EventKind};
use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::json;

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS review_approvals (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            change_id TEXT NOT NULL REFERENCES changes(id),
            reviewer TEXT NOT NULL,
            head TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            valid INTEGER NOT NULL CHECK (valid IN (0, 1))
        );
        CREATE INDEX IF NOT EXISTS approvals_by_change ON review_approvals(change_id, valid);",
    )?;
    // The reviewer's machine (R14); '' on approvals from before it was kept.
    changes::schema::add_text_column(conn, "review_approvals", "machine")
}

/// Records an approval from a client on the server's own machine.
pub fn approve(conn: &mut Connection, req: &ApproveRequest) -> Result<Approval, ReviewError> {
    approve_on(conn, req, crate::machines::signing::SERVER_MACHINE)
}

/// Records an approval from a reviewer on `machine`, which the caller took
/// from the connection (R14). A nick holding `@` could pass for another
/// machine's reviewer in the landed commit's trailer, so it is refused.
pub fn approve_on(
    conn: &mut Connection,
    req: &ApproveRequest,
    machine: &str,
) -> Result<Approval, ReviewError> {
    if req.reviewer.as_str().contains('@') {
        return Err(ReviewError::Invalid(format!(
            "reviewer {} must be a nick without '@'; the server records its machine",
            req.reviewer
        )));
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = changes::resolve(&tx, &req.change)?;
    let record = changes::get(&tx, id)?;
    super::check_reviewable(&record)?;
    super::check_head(Some(req.head), record.change.head)?;
    let now = super::store::now_millis();
    tx.execute(
        "INSERT INTO review_approvals (change_id, reviewer, head, created_at, valid, machine)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)",
        params![
            id.to_string(),
            req.reviewer.as_str(),
            req.head.to_hex(),
            now,
            machine
        ],
    )?;
    let approval = Approval {
        id: tx.last_insert_rowid() as u64,
        change: id,
        reviewer: req.reviewer.clone(),
        machine: Some(machine.to_owned()),
        head: req.head,
        created_at: now,
        valid: true,
    };
    // Staged with the approval, so a stop after the commit cannot lose it (R9).
    let detail = json!({"action": "approve", "head": approval.head, "by": approval.reviewer});
    let event = Staged::about(EventKind::Review, id, detail);
    crate::events::stage(&tx, &event)?;
    tx.commit()?;
    Ok(approval)
}

pub fn list(conn: &Connection, change: ChangeId) -> Result<Vec<Approval>, ReviewError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, reviewer, head, created_at, valid, machine FROM review_approvals
         WHERE change_id = ?1 ORDER BY id",
    )?;
    let rows = stmt
        .query_map([change.to_string()], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(id, reviewer, head, created_at, valid, machine)| {
            Ok(Approval {
                id,
                change,
                reviewer: super::store::agent(&reviewer, "approval reviewer")?,
                machine: Some(machine).filter(|machine| !machine.is_empty()),
                head: super::store::parse(&head, "approval head")?,
                created_at,
                valid,
            })
        })
        .collect()
}

/// Call inside the landing transaction to avoid racing an author snapshot.
/// A surviving approval may name the original head before a server rebase.
pub fn current(
    conn: &Connection,
    change: ChangeId,
    head: ObjectId,
) -> Result<Option<Approval>, ReviewError> {
    if changes::get(conn, change)?.change.head != head {
        return Ok(None);
    }
    Ok(list(conn, change)?.into_iter().rev().find(|a| a.valid))
}
