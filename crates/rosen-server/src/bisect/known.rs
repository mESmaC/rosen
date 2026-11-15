//! Search past recorded build culprits without calling a broken prefix green.
use rosen_core::ObjectId;
use rosen_protocol::{CheckKind, ErrorBody};
use rusqlite::{params, Connection, OptionalExtension};

pub(super) fn context(
    conn: &Connection,
    name: &str,
    green: ObjectId,
    failed: ObjectId,
    kind: CheckKind,
) -> Result<(ObjectId, Option<String>), ErrorBody> {
    if kind != CheckKind::Build {
        return Ok((green, None));
    }
    crate::post_land::failures::ensure_schema(conn).map_err(super::internal)?;
    let boundary: String = conn.query_row(
        "SELECT h.commit_id FROM base_history h WHERE h.ref_name=?1 AND
        (h.commit_id=?2 OR (h.seq < (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?3)
        AND (EXISTS(SELECT 1 FROM post_land_failures f WHERE f.commit_id=h.commit_id AND f.kind='build')
            OR EXISTS(SELECT 1 FROM bisections b WHERE b.ref_name=?1 AND b.failed=h.commit_id
                AND b.kind='build' AND b.phase='known')))) ORDER BY h.seq DESC LIMIT 1",
        params![name,green.to_hex(),failed.to_hex()], |r| r.get(0)).map_err(super::internal)?;
    let known = conn.query_row(
        "SELECT f.output FROM post_land_failures f JOIN base_history h ON h.commit_id=f.commit_id
        WHERE h.ref_name=?1 AND f.kind='build' AND f.requires_revert=1 AND f.revert IS NULL
        AND length(trim(f.output)) > 0
        AND NOT EXISTS(SELECT 1 FROM auto_reverts a WHERE a.culprit=f.commit_id)
        AND h.seq >= (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2)
        AND h.seq < (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?3)
        ORDER BY h.seq DESC LIMIT 1",
        params![name,green.to_hex(),failed.to_hex()], |r| r.get(0)).optional().map_err(super::internal)?;
    Ok((boundary.parse().map_err(super::internal)?, known))
}
