//! Refused inverses are durable operator signals, deduplicated at each base.
use super::{LandError, LandResult, Ledger};
use crate::{events::Staged, ServerState};
use rosen_core::ObjectId;
use rosen_protocol::EventKind;
use rosen_store::{LooseObjectStore, ObjectStore};
use rusqlite::{params, Connection, TransactionBehavior};

/// One row per culprit and attempted base. `paths`, `overlapping` and
/// `refused_at` were added for `post_land_status`; older rows hold none.
pub(crate) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS revert_refusals (
        culprit TEXT NOT NULL, base TEXT NOT NULL, reason TEXT NOT NULL,
        PRIMARY KEY(culprit,base));",
    )?;
    use crate::post_land::submitter::add_column;
    add_column(conn, "revert_refusals", "paths", "TEXT")?;
    add_column(conn, "revert_refusals", "overlapping", "TEXT")?;
    add_column(conn, "revert_refusals", "refused_at", "INTEGER")
}

pub(super) fn record(state: &ServerState, culprit: ObjectId, error: &LandError) -> LandResult<()> {
    let ledger = Ledger::open(&state.config.metadata_path())?;
    let Some((name, tip)) = ledger.project_base()? else {
        return Ok(());
    };
    let (base, paths) = match error {
        LandError::RevertConflict { base, paths } => (*base, paths.as_slice()),
        _ => (tip, [].as_slice()),
    };
    let mut conn = crate::metadata::open(&state.config.metadata_path())?;
    ensure_schema(&conn)?;
    let change: String = conn.query_row(
        "SELECT change_id FROM base_history WHERE ref_name=?1 AND commit_id=?2",
        params![name, culprit.to_hex()],
        |r| r.get(0),
    )?;
    let overlapping = overlaps(state, &conn, &name, culprit, base, paths)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO revert_refusals
        (culprit,base,reason,paths,overlapping,refused_at) VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            culprit.to_hex(),
            base.to_hex(),
            error.to_string(),
            serde_json::Value::from(paths).to_string(),
            serde_json::to_string(&overlapping).map_err(backend)?,
            crate::post_land::submitter::now_millis()
        ],
    )?;
    if inserted != 0 {
        let detail = serde_json::json!({"culprit":culprit,"base":base,
            "overlapping_commits":overlapping,"paths":paths,"reason":error.to_string()});
        crate::events::stage(
            &tx,
            &Staged::about(EventKind::RevertRefused, change, detail),
        )?;
    }
    tx.commit()?;
    state.events.deliver_staged();
    Ok(())
}

fn overlaps(
    state: &ServerState,
    conn: &rusqlite::Connection,
    name: &str,
    culprit: ObjectId,
    base: ObjectId,
    paths: &[String],
) -> LandResult<Vec<ObjectId>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let store = LooseObjectStore::open(state.config.objects_dir()).map_err(backend)?;
    let mut query = conn.prepare(
        "SELECT commit_id FROM base_history WHERE ref_name=?1
        AND seq > (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?2)
        AND seq <= (SELECT seq FROM base_history WHERE ref_name=?1 AND commit_id=?3)
        ORDER BY seq",
    )?;
    let commits = query
        .query_map(params![name, culprit.to_hex(), base.to_hex()], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut overlapping = Vec::new();
    for id in commits {
        let id: ObjectId = id.parse()?;
        let commit = store.read_commit(&id).map_err(backend)?;
        let before = commit
            .parents
            .first()
            .map(|id| store.read_commit(id))
            .transpose()
            .map_err(backend)?;
        let changes = crate::history::trees::changed_files(
            &store,
            before.map(|c| c.tree),
            Some(commit.tree),
            &[],
        )
        .map_err(backend)?;
        if changes.iter().any(|change| {
            paths.iter().any(|path| {
                change.path == *path
                    || change.path.starts_with(&format!("{path}/"))
                    || path.starts_with(&format!("{}/", change.path))
            })
        }) {
            overlapping.push(id);
        }
    }
    Ok(overlapping)
}

fn backend(error: impl std::fmt::Display) -> LandError {
    LandError::Backend(error.to_string())
}
