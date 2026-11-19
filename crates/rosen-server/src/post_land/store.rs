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
             JOIN bisections b ON b.kind=f.kind AND b.phase='done'
                 AND json_extract(b.commits,'$[' || b.high || '][0]')=f.commit_id
             JOIN base_history h ON h.ref_name=b.ref_name AND h.commit_id=b.failed
             WHERE b.ref_name=?1 AND f.kind='test' AND f.requires_revert=0),0)
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
    let (by, at) = (submitted.map(|s| &s.by), submitted.map(|s| s.at));
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let end: Option<i64> = tx
        .query_row(
            "SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2
         AND change_id IS NOT NULL",
            params![name, commit.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(end) = end else { return Ok(false) };
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
    }
    tx.commit()?;
    Ok(true)
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
