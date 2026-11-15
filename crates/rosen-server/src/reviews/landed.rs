//! The reviewed state of landed changes (SPEC R7). In a project where review
//! does not gate landing, reviewers mark each landed change once they have
//! read it, so the backlog of unreviewed lands stays visible.
//!
//! The state is keyed by the landed commit, not the change: a change that R12
//! reverts reopens and may land again, and that later land is new code that
//! starts unreviewed.

use super::ReviewError;
use crate::changes;
use crate::events::Staged;
use rosen_core::{AgentId, ChangeId, ChangeState, ObjectId, ReviewedState};
use rosen_protocol::{EventKind, LandedReview, MarkReviewed};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS landed_reviews (
             commit_id  TEXT    PRIMARY KEY,
             change_id  TEXT    NOT NULL REFERENCES changes(id),
             state      TEXT    NOT NULL
                 CHECK (state IN ('unreviewed', 'reviewed', 'issues_filed')),
             marked_by  TEXT    NOT NULL,
             updated_at INTEGER NOT NULL
         );",
    )
}

/// Landed commits on the base, with their recorded state. A land whose
/// commit R12 reverted is left out: its change is open again.
const LANDED: &str = "SELECT h.change_id, h.commit_id, h.tier, r.state, r.marked_by, r.updated_at
     FROM base_history h LEFT JOIN landed_reviews r ON r.commit_id = h.commit_id
     WHERE h.change_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM auto_reverts a WHERE a.culprit = h.commit_id)";

/// Sets the reviewed state of a landed change's current land, and stages a
/// `review` event about it in the same transaction (R9).
pub fn mark(conn: &mut Connection, req: &MarkReviewed) -> Result<LandedReview, ReviewError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = changes::resolve(&tx, &req.change)?;
    let record = changes::get(&tx, id)?;
    if record.change.state != ChangeState::Landed {
        return Err(ReviewError::Refused(format!(
            "change {id} is {}; only landed changes carry a reviewed state",
            record.change.state
        )));
    }
    let query = format!("{LANDED} AND h.change_id = ?1 ORDER BY h.seq DESC LIMIT 1");
    let landed = tx
        .query_row(&query, params![id.to_string()], read_row)
        .optional()?
        .ok_or_else(|| ReviewError::Store(format!("landed change {id} has no landed commit")))?;
    let now = super::store::now_millis();
    tx.execute(
        "INSERT INTO landed_reviews (commit_id, change_id, state, marked_by, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (commit_id) DO UPDATE
         SET state = excluded.state, marked_by = excluded.marked_by,
             updated_at = excluded.updated_at",
        params![landed.1, landed.0, req.state.as_str(), req.by.as_str(), now],
    )?;
    let review = LandedReview {
        state: req.state,
        by: Some(req.by.clone()),
        updated_at: Some(now),
        ..parse(landed)?
    };
    let detail = json!({
        "action": "mark",
        "state": review.state,
        "commit": review.commit,
        "by": review.by,
    });
    crate::events::stage(&tx, &Staged::about(EventKind::Review, id, detail))?;
    tx.commit()?;
    Ok(review)
}

/// Landed changes, oldest land first, optionally only those in `state`. A
/// land nobody has marked is `unreviewed`.
pub fn list(
    conn: &Connection,
    state: Option<ReviewedState>,
) -> Result<Vec<LandedReview>, ReviewError> {
    let query =
        format!("{LANDED} AND (?1 IS NULL OR COALESCE(r.state, 'unreviewed') = ?1) ORDER BY h.seq");
    let mut stmt = conn.prepare_cached(&query)?;
    let rows = stmt
        .query_map(params![state.map(ReviewedState::as_str)], read_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(parse).collect()
}

type Row = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
);

fn read_row(row: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn parse(row: Row) -> Result<LandedReview, ReviewError> {
    let (change, commit, tier, state, by, updated_at) = row;
    let change: ChangeId = super::store::parse(&change, "landed change")?;
    let commit: ObjectId = super::store::parse(&commit, "landed commit")?;
    let state = match state {
        Some(text) => super::store::parse(&text, "reviewed state")?,
        None => ReviewedState::Unreviewed,
    };
    let by: Option<AgentId> = by
        .map(|text| super::store::agent(&text, "reviewed state marker"))
        .transpose()?;
    Ok(LandedReview {
        change,
        commit,
        tier,
        state,
        by,
        updated_at,
    })
}
