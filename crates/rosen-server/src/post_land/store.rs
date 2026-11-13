//! A monotone green prefix and durable notifications in the metadata store.
use rosen_core::ObjectId;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS post_land_green (
             ref_name TEXT PRIMARY KEY, seq INTEGER NOT NULL, commit_id TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS post_land_delivery (
             seq INTEGER PRIMARY KEY, change_id TEXT NOT NULL, commit_id TEXT NOT NULL
         );",
    )
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
    let id: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM base_history WHERE ref_name=?1 AND change_id IS NOT NULL
         AND seq > COALESCE((SELECT seq FROM post_land_green WHERE ref_name=?1),0)
         ORDER BY seq DESC LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    id.map(parse).transpose()
}

/// The writer reservation serializes passes with each other and with lands.
/// The fixed upper bound excludes lands that arrived while tests were running.
pub fn passed(conn: &mut Connection, name: &str, commit: &ObjectId) -> rusqlite::Result<bool> {
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
            "INSERT OR IGNORE INTO post_land_delivery(seq,change_id,commit_id)
             SELECT seq,change_id,commit_id FROM base_history
             WHERE ref_name=?1 AND seq>?2 AND seq<=?3 AND change_id IS NOT NULL",
            params![name, start, end],
        )?;
        tx.execute(
            "INSERT INTO post_land_green(ref_name,seq,commit_id) VALUES (?1,?2,?3)
             ON CONFLICT(ref_name) DO UPDATE SET seq=excluded.seq,commit_id=excluded.commit_id",
            params![name, end, commit.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

pub fn pending(conn: &Connection) -> rusqlite::Result<Vec<(i64, String, String)>> {
    let mut stmt =
        conn.prepare("SELECT seq,change_id,commit_id FROM post_land_delivery ORDER BY seq")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

fn parse(id: String) -> rusqlite::Result<ObjectId> {
    id.parse()
        .map_err(|_| rusqlite::Error::InvalidParameterName("invalid green commit".into()))
}
