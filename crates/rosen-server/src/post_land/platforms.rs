//! Per-platform post-land checkpoints and aggregate base advancement (R12, #440).
//!
//! Each required platform keeps its own monotone passing prefix in
//! `post_land_platform_green`. The base checkpoint (`post_land_green` for tests,
//! `post_land_build_green` for builds) advances only to the minimum passing
//! position across every required platform, in the same transaction.

use super::submitter::Submitted;
use rosen_core::{ObjectId, Platform};
use rosen_protocol::{CheckKind, Checkpoint};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS post_land_platform_green (
             ref_name TEXT NOT NULL,
             platform TEXT NOT NULL,
             kind TEXT NOT NULL CHECK(kind IN ('build','test')),
             seq INTEGER NOT NULL,
             commit_id TEXT NOT NULL,
             submitter TEXT,
             submitted_at INTEGER,
             PRIMARY KEY (ref_name, platform, kind)
         );",
    )
}

pub fn checkpoint(
    conn: &Connection,
    name: &str,
    platform: &str,
    kind: CheckKind,
) -> rusqlite::Result<Option<Checkpoint>> {
    let row: Option<(String, Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT commit_id, submitter, submitted_at FROM post_land_platform_green
             WHERE ref_name=?1 AND platform=?2 AND kind=?3",
            params![name, platform, kind.as_str()],
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

pub fn last_green(
    conn: &Connection,
    name: &str,
    platform: &str,
    kind: CheckKind,
) -> rusqlite::Result<Option<ObjectId>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM post_land_platform_green
             WHERE ref_name=?1 AND platform=?2 AND kind=?3",
            params![name, platform, kind.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    id.map(parse).transpose()
}

pub fn newest(
    conn: &Connection,
    name: &str,
    platform: &str,
    kind: CheckKind,
) -> rusqlite::Result<Option<ObjectId>> {
    ensure_schema(conn)?;
    crate::bisect::ensure_schema(conn)?;
    super::failures::ensure_schema(conn)?;
    let id: Option<String> = match kind {
        CheckKind::Test => conn
            .query_row(
                "SELECT commit_id FROM base_history WHERE ref_name=?1 AND change_id IS NOT NULL
                 AND seq > COALESCE((SELECT seq FROM post_land_platform_green
                     WHERE ref_name=?1 AND platform=?2 AND kind='test'), 0)
                 AND seq > COALESCE((SELECT MAX(h.seq) FROM post_land_failures f
                     JOIN bisections b ON b.kind=f.kind AND b.platform=f.platform AND b.phase='done'
                         AND json_extract(b.commits,'$[' || b.high || '][0]')=f.commit_id
                     JOIN base_history h ON h.ref_name=b.ref_name AND h.commit_id=b.failed
                     WHERE b.ref_name=?1 AND b.platform=?2 AND f.kind='test' AND f.requires_revert=0), 0)
                 AND seq > COALESCE((SELECT MAX(h.seq) FROM bisections b
                     JOIN base_history h ON h.ref_name=b.ref_name AND h.commit_id=b.failed
                     WHERE b.ref_name=?1 AND b.platform=?2 AND b.kind='test' AND b.phase='known'), 0)
                 ORDER BY seq DESC LIMIT 1",
                params![name, platform],
                |r| r.get(0),
            )
            .optional()?,
        CheckKind::Build => conn
            .query_row(
                "SELECT commit_id FROM base_history WHERE ref_name=?1 AND tier NOT IN ('initial','mirror')
                 AND seq > COALESCE((SELECT seq FROM post_land_platform_green
                     WHERE ref_name=?1 AND platform=?2 AND kind='build'), 0)
                 AND seq > COALESCE((SELECT MAX(h.seq) FROM bisections b
                     JOIN base_history h ON h.ref_name=b.ref_name AND h.commit_id=b.failed
                     WHERE b.ref_name=?1 AND b.platform=?2 AND b.kind='build'
                         AND b.phase IN ('done','known')), 0)
                 ORDER BY seq DESC LIMIT 1",
                params![name, platform],
                |r| r.get(0),
            )
            .optional()?,
    };
    id.map(parse).transpose()
}

/// Records one platform's passing run and, when every platform in `required`
/// has now passed at or past a newer commit, advances the aggregate base
/// checkpoint to their minimum passing commit atomically, invoking `on_green`
/// inside the same write transaction when the overall test prefix advances.
#[allow(clippy::too_many_arguments)]
pub fn passed_with(
    conn: &mut Connection,
    name: &str,
    platform: &str,
    kind: CheckKind,
    commit: &ObjectId,
    required: &[Platform],
    submitted: Option<&Submitted>,
    on_green: impl FnOnce(&Connection, i64, i64, &ObjectId) -> Result<(), rosen_protocol::ErrorBody>,
) -> Result<bool, rosen_protocol::ErrorBody> {
    let sql_err = |e: rusqlite::Error| {
        rosen_protocol::ErrorBody::new(rosen_protocol::ErrorCode::Internal, e.to_string())
    };
    ensure_schema(conn).map_err(sql_err)?;
    let (by, at) = (submitted.map(|s| &s.by), submitted.map(|s| s.at));
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let end = history_seq(&tx, name, commit, kind).map_err(sql_err)?;
    let Some(end) = end else { return Ok(false) };
    let prev_seq: i64 = tx
        .query_row(
            "SELECT COALESCE((SELECT seq FROM post_land_platform_green
         WHERE ref_name=?1 AND platform=?2 AND kind=?3), 0)",
            params![name, platform, kind.as_str()],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    let updated = tx
        .execute(
            "INSERT INTO post_land_platform_green
             (ref_name, platform, kind, seq, commit_id, submitter, submitted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(ref_name, platform, kind) DO UPDATE SET
                 seq=excluded.seq, commit_id=excluded.commit_id,
                 submitter=excluded.submitter, submitted_at=excluded.submitted_at
             WHERE excluded.seq > post_land_platform_green.seq",
            params![name, platform, kind.as_str(), end, commit.to_hex(), by, at],
        )
        .map_err(sql_err)?;
    if updated > 0 {
        let batch_size = crate::stats::store::batch_size_between(&tx, name, kind, prev_seq, end)
            .map_err(sql_err)?;
        crate::stats::store::record_checkpoint_passed(
            &tx,
            name,
            kind,
            Some(platform),
            commit,
            batch_size,
            at.unwrap_or_else(crate::stats::now_millis),
        )
        .map_err(sql_err)?;
    }
    if let Some((overall_seq, overall_commit)) =
        aggregate_green(&tx, name, kind, required).map_err(sql_err)?
    {
        match kind {
            CheckKind::Test => {
                if let Some((prev, end)) =
                    super::store::advance_in(&tx, name, overall_seq, &overall_commit, submitted)
                        .map_err(sql_err)?
                {
                    on_green(&tx, prev, end, &overall_commit)?;
                }
            }
            CheckKind::Build => {
                super::builds::advance_in(&tx, name, overall_seq, &overall_commit, submitted)
                    .map_err(sql_err)?;
            }
        }
    }
    tx.commit().map_err(sql_err)?;
    Ok(true)
}

fn history_seq(
    tx: &Connection,
    name: &str,
    commit: &ObjectId,
    kind: CheckKind,
) -> rusqlite::Result<Option<i64>> {
    let sql = match kind {
        CheckKind::Test => {
            "SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2
             AND change_id IS NOT NULL"
        }
        CheckKind::Build => "SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2",
    };
    tx.query_row(sql, params![name, commit.to_hex()], |r| r.get(0))
        .optional()
}

fn aggregate_green(
    tx: &Connection,
    name: &str,
    kind: CheckKind,
    required: &[Platform],
) -> rusqlite::Result<Option<(i64, ObjectId)>> {
    if required.is_empty() {
        return Ok(None);
    }
    let mut min_seq: Option<i64> = None;
    for target in required {
        let seq: Option<i64> = tx
            .query_row(
                "SELECT seq FROM post_land_platform_green
                 WHERE ref_name=?1 AND platform=?2 AND kind=?3",
                params![name, target.as_str(), kind.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(seq) = seq else {
            return Ok(None);
        };
        min_seq = Some(min_seq.map_or(seq, |current| current.min(seq)));
    }
    let Some(seq) = min_seq else {
        return Ok(None);
    };
    let commit: Option<String> = tx
        .query_row(
            "SELECT commit_id FROM base_history WHERE ref_name=?1 AND seq=?2",
            params![name, seq],
            |r| r.get(0),
        )
        .optional()?;
    commit.map(|hex| Ok((seq, parse(hex)?))).transpose()
}

fn parse(id: String) -> rusqlite::Result<ObjectId> {
    id.parse()
        .map_err(|_| rusqlite::Error::InvalidParameterName("invalid green commit".into()))
}
