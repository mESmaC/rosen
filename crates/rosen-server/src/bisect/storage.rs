use super::model::Session;
use rosen_protocol::{ErrorBody, ErrorCode};
use rusqlite::{params, Connection, OptionalExtension};

pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS bisections (
        id INTEGER PRIMARY KEY, ref_name TEXT NOT NULL,
        failed TEXT NOT NULL, commits TEXT NOT NULL,
        low INTEGER NOT NULL, high INTEGER NOT NULL,
        phase TEXT NOT NULL, step INTEGER NOT NULL, output TEXT NOT NULL,
        UNIQUE(ref_name,failed));",
    )
}

pub(super) fn load(conn: &Connection, id: i64) -> Result<Session, ErrorBody> {
    let row = conn
        .query_row(
            "SELECT commits,low,high,phase,step,output FROM bisections WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(super::internal)?
        .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "unknown bisection"))?;
    Ok(Session {
        id,
        commits: serde_json::from_str(&row.0).map_err(super::internal)?,
        low: row.1,
        high: row.2,
        phase: row.3,
        step: row.4,
        output: row.5,
    })
}

pub(super) fn save(conn: &Connection, session: &Session) -> Result<(), ErrorBody> {
    conn.execute(
        "UPDATE bisections SET low=?2,high=?3,phase=?4,step=?5,output=?6 WHERE id=?1",
        params![
            session.id,
            session.low,
            session.high,
            session.phase,
            session.step,
            session.output
        ],
    )
    .map_err(super::internal)?;
    Ok(())
}
