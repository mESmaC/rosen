//! A monotone green prefix and durable notifications in the metadata store.
use super::submitter::{self, Submitted};
use rosen_core::ObjectId;
use rosen_protocol::Checkpoint;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS post_land_green (
             ref_name TEXT PRIMARY KEY, seq INTEGER NOT NULL, commit_id TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS post_land_delivery (
             seq INTEGER PRIMARY KEY, change_id TEXT NOT NULL, commit_id TEXT NOT NULL
         );",
    )?;
    submitter::add_columns(conn, "post_land_green")?;
    submitter::add_columns(conn, "post_land_delivery")
}

/// The test checkpoint with who reported it, for the read-only status.
pub fn checkpoint(conn: &Connection, name: &str) -> rusqlite::Result<Option<Checkpoint>> {
    checkpoint_in(conn, "post_land_green", name)
}

/// Reads `table`'s checkpoint row; `table` is one of the two checkpoint tables.
pub(super) fn checkpoint_in(
    conn: &Connection,
    table: &str,
    name: &str,
) -> rusqlite::Result<Option<Checkpoint>> {
    let row: Option<(String, Option<String>, Option<i64>)> = conn
        .query_row(
            &format!("SELECT commit_id,submitter,submitted_at FROM {table} WHERE ref_name=?1"),
            [name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(commit, submitter, at)| {
        Ok(Checkpoint {
            commit: parse(commit)?,
            submitter,
            at,
        })
    })
    .transpose()
}

pub fn last_green(conn: &Connection, name: &str) -> rusqlite::Result<Option<ObjectId>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM post_land_green WHERE ref_name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    id.map(parse).transpose()
}

pub fn newest(conn: &Connection, name: &str) -> rusqlite::Result<Option<ObjectId>> {
    crate::bisect::ensure_schema(conn)?;
    super::failures::ensure_schema(conn)?;
    let id: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM base_history WHERE ref_name=?1 AND change_id IS NOT NULL
         AND seq > COALESCE((SELECT seq FROM post_land_green WHERE ref_name=?1),0)
         AND seq > COALESCE((SELECT MAX(h.seq) FROM post_land_failures f
             JOIN bisections b ON b.kind=f.kind AND b.platform=f.platform AND b.phase='done'
                 AND json_extract(b.commits,'$[' || b.high || '][0]')=f.commit_id
             JOIN base_history h ON h.ref_name=b.ref_name AND h.commit_id=b.failed
             WHERE b.ref_name=?1 AND b.platform='' AND f.kind='test' AND f.requires_revert=0),0)
         ORDER BY seq DESC LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    id.map(parse).transpose()
}

pub fn passed(conn: &mut Connection, name: &str, commit: &ObjectId) -> rusqlite::Result<bool> {
    passed_by(conn, name, commit, None)
}

/// The writer reservation serializes passes with each other and with lands.
/// The fixed upper bound excludes lands that arrived while tests were running.
pub fn passed_by(
    conn: &mut Connection,
    name: &str,
    commit: &ObjectId,
    submitted: Option<&Submitted>,
) -> rusqlite::Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let advanced = advance_green(&tx, name, commit, submitted)?;
    tx.commit()?;
    Ok(advanced.is_some())
}

/// Like [`passed_by`], running `on_advance(tx, prev_seq, end_seq)` inside the
/// same write transaction whenever the green prefix moves forward.
pub(super) fn passed_by_with(
    conn: &mut Connection,
    name: &str,
    commit: &ObjectId,
    submitted: Option<&Submitted>,
    on_advance: impl FnOnce(&Connection, i64, i64) -> Result<(), rosen_protocol::ErrorBody>,
) -> Result<bool, rosen_protocol::ErrorBody> {
    let sql_err = |e: rusqlite::Error| {
        rosen_protocol::ErrorBody::new(rosen_protocol::ErrorCode::Internal, e.to_string())
    };
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let Some(range) = advance_green(&tx, name, commit, submitted).map_err(sql_err)? else {
        return Ok(false);
    };
    if let Some((start, end)) = range {
        on_advance(&tx, start, end)?;
    }
    tx.commit().map_err(sql_err)?;
    Ok(true)
}

fn advance_green(
    tx: &Connection,
    name: &str,
    commit: &ObjectId,
    submitted: Option<&Submitted>,
) -> rusqlite::Result<Option<Option<(i64, i64)>>> {
    let end: Option<i64> = tx
        .query_row(
            "SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2
         AND change_id IS NOT NULL",
            params![name, commit.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(end) = end else { return Ok(None) };
    Ok(Some(advance_in(tx, name, end, commit, submitted)?))
}

/// Advances the overall test green checkpoint and stages green deliveries in an
/// existing write transaction. Returns `Some((prev_seq, end_seq))` when the
/// green prefix moved forward.
pub(super) fn advance_in(
    tx: &Connection,
    name: &str,
    end: i64,
    commit: &ObjectId,
    submitted: Option<&Submitted>,
) -> rusqlite::Result<Option<(i64, i64)>> {
    let (by, at) = (submitted.map(|s| &s.by), submitted.map(|s| s.at));
    let start: i64 = tx.query_row(
        "SELECT COALESCE((SELECT seq FROM post_land_green WHERE ref_name=?1),0)",
        [name],
        |r| r.get(0),
    )?;
    if end > start {
        tx.execute(
            "INSERT OR IGNORE INTO post_land_delivery
             (seq,change_id,commit_id,submitter,submitted_at)
             SELECT seq,change_id,commit_id,?4,?5 FROM base_history
             WHERE ref_name=?1 AND seq>?2 AND seq<=?3 AND change_id IS NOT NULL",
            params![name, start, end, by, at],
        )?;
        tx.execute(
            "INSERT INTO post_land_green(ref_name,seq,commit_id,submitter,submitted_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(ref_name) DO UPDATE SET seq=excluded.seq,commit_id=excluded.commit_id,
             submitter=excluded.submitter,submitted_at=excluded.submitted_at",
            params![name, end, commit.to_string(), by, at],
        )?;
        let batch_size = crate::stats::store::batch_size_between(
            tx,
            name,
            rosen_protocol::CheckKind::Test,
            start,
            end,
        )?;
        crate::stats::store::record_checkpoint_passed(
            tx,
            name,
            rosen_protocol::CheckKind::Test,
            None,
            commit,
            batch_size,
            at.unwrap_or_else(crate::stats::now_millis),
        )?;
        return Ok(Some((start, end)));
    }
    Ok(None)
}

/// One `green` notification still to publish for a landed change.
pub struct Delivery {
    pub seq: i64,
    pub change: String,
    pub commit: String,
    pub submitter: Option<String>,
    pub at: Option<i64>,
}

pub fn pending(conn: &Connection) -> rusqlite::Result<Vec<Delivery>> {
    let mut stmt = conn.prepare(
        "SELECT seq,change_id,commit_id,submitter,submitted_at
         FROM post_land_delivery ORDER BY seq",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Delivery {
            seq: r.get(0)?,
            change: r.get(1)?,
            commit: r.get(2)?,
            submitter: r.get(3)?,
            at: r.get(4)?,
        })
    })?;
    rows.collect()
}

fn parse(id: String) -> rusqlite::Result<ObjectId> {
    id.parse()
        .map_err(|_| rusqlite::Error::InvalidParameterName("invalid green commit".into()))
}
