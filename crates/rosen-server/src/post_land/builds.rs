//! Build progress is separate from the full-test green prefix.
use rosen_core::ObjectId;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS post_land_build_green (
        ref_name TEXT PRIMARY KEY, seq INTEGER NOT NULL, commit_id TEXT NOT NULL);",
    )
}

pub fn last_green(conn: &Connection, name: &str) -> rusqlite::Result<Option<ObjectId>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM post_land_build_green WHERE ref_name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    text.map(|id| {
        id.parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName(id))
    })
    .transpose()
}

pub fn newest(conn: &Connection, name: &str) -> rusqlite::Result<Option<ObjectId>> {
    crate::bisect::ensure_schema(conn)?;
    let text: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM base_history WHERE ref_name=?1 AND tier NOT IN ('initial','mirror')
         AND seq > COALESCE((SELECT seq FROM post_land_build_green WHERE ref_name=?1),0)
         AND seq > COALESCE((SELECT MAX(h.seq) FROM bisections b
             JOIN base_history h ON h.ref_name=b.ref_name AND h.commit_id=b.failed
             WHERE b.ref_name=?1 AND b.kind='build' AND b.phase IN ('done','known')),0)
         ORDER BY seq DESC LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    text.map(|id| {
        id.parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName(id))
    })
    .transpose()
}

pub fn passed(conn: &mut Connection, name: &str, commit: &ObjectId) -> rusqlite::Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let count = tx.execute(
        "INSERT INTO post_land_build_green(ref_name,seq,commit_id)
         SELECT ref_name,seq,commit_id FROM base_history
         WHERE ref_name=?1 AND commit_id=?2
         ON CONFLICT(ref_name) DO UPDATE SET seq=excluded.seq,commit_id=excluded.commit_id
         WHERE excluded.seq > post_land_build_green.seq",
        params![name, commit.to_string()],
    )?;
    if count > 0 {
        crate::events::stage(
            &tx,
            &crate::events::Staged {
                kind: rosen_protocol::EventKind::Built,
                change: None,
                detail: serde_json::json!({ "commit": commit }),
                concern: Default::default(),
            },
        )?;
    }
    let known = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM base_history WHERE ref_name=?1 AND commit_id=?2)",
        params![name, commit.to_string()],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(known)
}
