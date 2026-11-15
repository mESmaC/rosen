//! Build progress is separate from the full-test green prefix.
use rosen_core::ObjectId;
use rusqlite::{params, Connection, OptionalExtension};

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
    let text: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM base_history WHERE ref_name=?1 AND change_id IS NOT NULL
         AND seq > COALESCE((SELECT seq FROM post_land_build_green WHERE ref_name=?1),0)
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

pub fn passed(conn: &Connection, name: &str, commit: &ObjectId) -> rusqlite::Result<bool> {
    let count = conn.execute(
        "INSERT INTO post_land_build_green(ref_name,seq,commit_id)
         SELECT ref_name,seq,commit_id FROM base_history
         WHERE ref_name=?1 AND commit_id=?2 AND change_id IS NOT NULL
         ON CONFLICT(ref_name) DO UPDATE SET seq=excluded.seq,commit_id=excluded.commit_id
         WHERE excluded.seq > post_land_build_green.seq",
        params![name, commit.to_string()],
    )?;
    Ok(count > 0
        || conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM base_history
        WHERE ref_name=?1 AND commit_id=?2 AND change_id IS NOT NULL)",
            params![name, commit.to_string()],
            |r| r.get(0),
        )?)
}
