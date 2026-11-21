//! Build progress is separate from the full-test green prefix.
use super::submitter::{self, Submitted};
use rosen_core::ObjectId;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS post_land_build_green (
        ref_name TEXT PRIMARY KEY, seq INTEGER NOT NULL, commit_id TEXT NOT NULL);",
    )?;
    submitter::add_columns(conn, "post_land_build_green")
}

/// The build checkpoint with who reported it, for the read-only status.
pub fn checkpoint(
    conn: &Connection,
    name: &str,
) -> rusqlite::Result<Option<rosen_protocol::Checkpoint>> {
    super::store::checkpoint_in(conn, "post_land_build_green", name)
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

pub fn passed(
    conn: &mut Connection,
    name: &str,
    commit: &ObjectId,
    submitted: Option<&Submitted>,
) -> rusqlite::Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (by, at) = (submitted.map(|s| &s.by), submitted.map(|s| s.at));
    let count = tx.execute(
        "INSERT INTO post_land_build_green(ref_name,seq,commit_id,submitter,submitted_at)
         SELECT ref_name,seq,commit_id,?3,?4 FROM base_history
         WHERE ref_name=?1 AND commit_id=?2
         ON CONFLICT(ref_name) DO UPDATE SET seq=excluded.seq,commit_id=excluded.commit_id,
         submitter=excluded.submitter,submitted_at=excluded.submitted_at
         WHERE excluded.seq > post_land_build_green.seq",
        params![name, commit.to_string(), by, at],
    )?;
    if count > 0 {
        crate::events::stage(
            &tx,
            &crate::events::Staged {
                kind: rosen_protocol::EventKind::Built,
                change: None,
                detail: event_detail(commit, submitted),
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

/// Event detail published when a post-land build passes (R9, R12).
pub(crate) fn event_detail(commit: &ObjectId, submitted: Option<&Submitted>) -> serde_json::Value {
    let (submitter, at) = submitter::detail(submitted);
    serde_json::json!({ "commit": commit, "submitter": submitter, "at": at })
}
