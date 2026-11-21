//! Post-land deploy step scheduling and result recording (SPEC R12).
//!
//! When a full-test green checkpoint advances, matching `[deploy.<name>]` steps
//! from `rosen.toml` at that commit are queued atomically with the green update.
//! One step runs at a time per project in `(green_seq, declaration)` order;
//! pending steps covered by a newer green are skipped before dispatch.

use super::submitter::Submitted;
use crate::events::{self, Staged};
use crate::ServerState;
use rosen_core::{DeployStep, ObjectId};
use rosen_protocol::{
    DeployRecord, DeployStatus, ErrorBody, ErrorCode, EventKind, Outcome, Response,
    SubmitDeployResult,
};
use rosen_store::{LooseObjectStore, ObjectStore};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

type Result<T> = std::result::Result<T, ErrorBody>;

fn internal(err: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, err.to_string())
}

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS deploy_runs (
             id            INTEGER PRIMARY KEY AUTOINCREMENT,
             ref_name      TEXT    NOT NULL,
             green_seq     INTEGER NOT NULL,
             step_index    INTEGER NOT NULL,
             step          TEXT    NOT NULL,
             commit_id     TEXT    NOT NULL,
             from_commit   TEXT,
             command       TEXT    NOT NULL,
             paths         TEXT    NOT NULL,
             secrets       TEXT    NOT NULL,
             platforms     TEXT    NOT NULL,
             state         TEXT    NOT NULL CHECK(state IN ('pending','running','passed','failed','skipped')),
             superseded_by TEXT,
             output        TEXT    NOT NULL DEFAULT '',
             submitter     TEXT,
             submitted_at  INTEGER,
             UNIQUE(ref_name, green_seq, step)
         );
         CREATE INDEX IF NOT EXISTS deploy_runs_queue
             ON deploy_runs(ref_name, state, green_seq, step_index, id);",
    )
}

/// Queues due deploy steps for the newly green range `(prev_seq, end_seq]` and
/// stages `deploy_due` if no step is currently running. Runs inside the
/// transaction that advances `post_land_green`.
pub(super) fn on_green_range(
    tx: &Connection,
    objects_dir: &Path,
    ref_name: &str,
    prev_seq: i64,
    end_seq: i64,
    commit: &ObjectId,
    steps: &[DeployStep],
) -> Result<()> {
    ensure_schema(tx).map_err(internal)?;
    let commit_hex = commit.to_hex();
    skip_removed_steps(tx, ref_name, end_seq, &commit_hex, steps)?;
    if steps.is_empty() {
        return Ok(());
    }
    let store = LooseObjectStore::open(objects_dir).map_err(internal)?;
    let from_commit = range_base_commit(tx, &store, ref_name, prev_seq, end_seq)?;
    let changed = range_changed_paths(&store, from_commit.as_ref(), commit)?;
    for (index, step) in steps.iter().enumerate() {
        if !step.matches_any(&changed) {
            continue;
        }
        let effective_from =
            supersede_pending(tx, ref_name, &step.name, end_seq, &commit_hex)?.or(from_commit);
        insert_pending(
            tx,
            ref_name,
            end_seq,
            i64::try_from(index).unwrap_or(i64::MAX),
            step,
            &commit_hex,
            effective_from.as_ref(),
        )?;
    }
    dispatch_next_if_idle(tx, ref_name)
}

/// Handles [`rosen_protocol::Request::DeployResult`] on the blocking pool.
pub async fn handle_result(
    state: &Arc<ServerState>,
    req: SubmitDeployResult,
    submitted: Submitted,
) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || record_result(&state, req, submitted)).await {
        Ok(Ok(record)) => Outcome::Ok(Response::DeployRecorded(Box::new(record))),
        Ok(Err(err)) => Outcome::Error(err),
        Err(err) => Outcome::Error(internal(err)),
    }
}

/// Marks pending/running deploy runs for rewritten commits as skipped so a
/// mirror history rewrite never leaves stale deploys queued.
pub fn invalidate_rewritten(conn: &Connection) -> rusqlite::Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='deploy_runs')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    conn.execute(
        "UPDATE deploy_runs
         SET state='skipped',
             output='Superseded by mirror history rewrite.'
         WHERE state IN ('pending','running')
           AND commit_id IN (SELECT old FROM rewrite_map)",
        [],
    )?;
    Ok(())
}

fn record_result(
    state: &ServerState,
    req: SubmitDeployResult,
    submitted: Submitted,
) -> Result<DeployRecord> {
    let mut conn = super::open(state)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    let row = load_run(&tx, req.id)?.ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::NotFound,
            format!("unknown deploy run {}", req.id),
        )
    })?;
    if row.step != req.step || row.commit != req.commit {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "deploy result does not match the offered step or commit",
        ));
    }
    let record = match row.state.as_str() {
        "pending" => {
            return Err(ErrorBody::new(
                ErrorCode::Refused,
                "deploy step has not been dispatched yet",
            ))
        }
        "skipped" if row.superseded_by.is_some() => {
            return Err(ErrorBody::new(
                ErrorCode::Refused,
                "deploy step was skipped because a newer green already covers it",
            ))
        }
        "passed" | "failed" | "skipped" => existing_or_refuse(&row, req.status)?,
        "running" => finish_running(&tx, &row, &req, &submitted)?,
        other => return Err(internal(format!("unexpected deploy run state {other:?}"))),
    };
    tx.commit().map_err(internal)?;
    state.events.deliver_staged();
    Ok(record)
}

fn existing_or_refuse(row: &DeployRow, status: DeployStatus) -> Result<DeployRecord> {
    if row.state == status.as_str() {
        return Ok(row.to_record(status));
    }
    Err(ErrorBody::new(
        ErrorCode::Refused,
        format!("deploy run {} already finished as {}", row.id, row.state),
    ))
}

fn finish_running(
    tx: &Connection,
    row: &DeployRow,
    req: &SubmitDeployResult,
    submitted: &Submitted,
) -> Result<DeployRecord> {
    tx.execute(
        "UPDATE deploy_runs
         SET state=?2, output=?3, submitter=?4, submitted_at=?5
         WHERE id=?1",
        params![
            row.id,
            req.status.as_str(),
            req.output,
            submitted.by,
            submitted.at
        ],
    )
    .map_err(internal)?;
    let record = DeployRecord {
        id: row.id,
        step: row.step.clone(),
        commit: row.commit,
        from: row.from,
        command: row.command.clone(),
        status: req.status,
        output: req.output.clone(),
        submitter: Some(submitted.by.clone()),
        submitted_at: Some(submitted.at),
    };
    events::stage(
        tx,
        &Staged {
            kind: EventKind::DeployResult,
            change: None,
            detail: result_detail(&record),
            concern: Default::default(),
        },
    )
    .map_err(internal)?;
    dispatch_next_if_idle(tx, &row.ref_name)?;
    Ok(record)
}

fn skip_removed_steps(
    tx: &Connection,
    ref_name: &str,
    end_seq: i64,
    commit_hex: &str,
    steps: &[DeployStep],
) -> Result<()> {
    let active: BTreeSet<&str> = steps.iter().map(|s| s.name.as_str()).collect();
    let mut stmt = tx
        .prepare(
            "SELECT id, step FROM deploy_runs
             WHERE ref_name=?1 AND state='pending' AND green_seq<?2",
        )
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![ref_name, end_seq], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(internal)?;
    for row in rows {
        let (id, step) = row.map_err(internal)?;
        if !active.contains(step.as_str()) {
            tx.execute(
                "UPDATE deploy_runs SET state='skipped', superseded_by=?2 WHERE id=?1",
                params![id, commit_hex],
            )
            .map_err(internal)?;
        }
    }
    Ok(())
}

/// Marks pending runs of `step` from earlier green checkpoints as skipped,
/// returning the earliest superseded `from_commit` so the replacement covers
/// the full undeployed range.
fn supersede_pending(
    tx: &Connection,
    ref_name: &str,
    step: &str,
    end_seq: i64,
    commit_hex: &str,
) -> Result<Option<ObjectId>> {
    let earliest: Option<Option<String>> = tx
        .query_row(
            "SELECT from_commit FROM deploy_runs
             WHERE ref_name=?1 AND step=?2 AND state='pending' AND green_seq<?3
             ORDER BY green_seq ASC, id ASC LIMIT 1",
            params![ref_name, step, end_seq],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?;
    tx.execute(
        "UPDATE deploy_runs SET state='skipped', superseded_by=?4
         WHERE ref_name=?1 AND step=?2 AND state='pending' AND green_seq<?3",
        params![ref_name, step, end_seq, commit_hex],
    )
    .map_err(internal)?;
    earliest
        .flatten()
        .map(|hex| hex.parse().map_err(internal))
        .transpose()
}

fn insert_pending(
    tx: &Connection,
    ref_name: &str,
    green_seq: i64,
    step_index: i64,
    step: &DeployStep,
    commit_hex: &str,
    from_commit: Option<&ObjectId>,
) -> Result<()> {
    let paths = serde_json::to_string(&step.paths).map_err(internal)?;
    let secrets = serde_json::to_string(&step.secrets).map_err(internal)?;
    let platforms = serde_json::to_string(&step.platforms).map_err(internal)?;
    tx.execute(
        "INSERT OR IGNORE INTO deploy_runs
         (ref_name,green_seq,step_index,step,commit_id,from_commit,command,paths,secrets,platforms,state)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'pending')",
        params![
            ref_name,
            green_seq,
            step_index,
            step.name,
            commit_hex,
            from_commit.map(ObjectId::to_hex),
            step.command,
            paths,
            secrets,
            platforms,
        ],
    )
    .map_err(internal)?;
    Ok(())
}

/// Dispatches the next pending step when no step is currently running for this
/// project, staging its `deploy_due` event in the same transaction.
fn dispatch_next_if_idle(tx: &Connection, ref_name: &str) -> Result<()> {
    let running: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM deploy_runs WHERE ref_name=?1 AND state='running')",
            [ref_name],
            |r| r.get(0),
        )
        .map_err(internal)?;
    if running {
        return Ok(());
    }
    let Some(next) = next_pending(tx, ref_name)? else {
        return Ok(());
    };
    tx.execute(
        "UPDATE deploy_runs SET state='running' WHERE id=?1",
        [next.id],
    )
    .map_err(internal)?;
    events::stage(
        tx,
        &Staged {
            kind: EventKind::DeployDue,
            change: None,
            detail: due_detail(&next)?,
            concern: Default::default(),
        },
    )
    .map_err(internal)
}

fn range_base_commit(
    tx: &Connection,
    store: &dyn ObjectStore,
    ref_name: &str,
    prev_seq: i64,
    end_seq: i64,
) -> Result<Option<ObjectId>> {
    if prev_seq > 0 {
        let id: Option<String> = tx
            .query_row(
                "SELECT commit_id FROM base_history WHERE ref_name=?1 AND seq=?2",
                params![ref_name, prev_seq],
                |r| r.get(0),
            )
            .optional()
            .map_err(internal)?;
        return id.map(|hex| hex.parse().map_err(internal)).transpose();
    }
    let first: Option<String> = tx
        .query_row(
            "SELECT commit_id FROM base_history
             WHERE ref_name=?1 AND seq>?2 AND seq<=?3 AND change_id IS NOT NULL
             ORDER BY seq ASC LIMIT 1",
            params![ref_name, prev_seq, end_seq],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?;
    let Some(first) = first else {
        return Ok(None);
    };
    let first_id: ObjectId = first.parse().map_err(internal)?;
    let commit = store.read_commit(&first_id).map_err(internal)?;
    Ok(commit.parents.first().copied())
}

fn range_changed_paths(
    store: &dyn ObjectStore,
    from: Option<&ObjectId>,
    to: &ObjectId,
) -> Result<Vec<String>> {
    let old_tree = from
        .map(|id| store.read_commit(id).map(|c| c.tree))
        .transpose()
        .map_err(internal)?;
    let new_tree = store.read_commit(to).map_err(internal)?.tree;
    let changes = crate::history::trees::changed_files(store, old_tree, Some(new_tree), &[])
        .map_err(internal)?;
    Ok(changes.into_iter().map(|c| c.path).collect())
}

struct DeployRow {
    id: i64,
    ref_name: String,
    step: String,
    commit: ObjectId,
    from: Option<ObjectId>,
    command: String,
    paths: String,
    secrets: String,
    platforms: String,
    state: String,
    superseded_by: Option<String>,
    output: String,
    submitter: Option<String>,
    submitted_at: Option<i64>,
}

impl DeployRow {
    fn to_record(&self, status: DeployStatus) -> DeployRecord {
        DeployRecord {
            id: self.id,
            step: self.step.clone(),
            commit: self.commit,
            from: self.from,
            command: self.command.clone(),
            status,
            output: self.output.clone(),
            submitter: self.submitter.clone(),
            submitted_at: self.submitted_at,
        }
    }
}

const SELECT_ROW: &str = "SELECT id,ref_name,step,commit_id,from_commit,command,paths,secrets,\
     platforms,state,superseded_by,output,submitter,submitted_at FROM deploy_runs";

fn load_run(conn: &Connection, id: i64) -> Result<Option<DeployRow>> {
    let sql = format!("{SELECT_ROW} WHERE id=?1");
    conn.query_row(&sql, [id], read_row)
        .optional()
        .map_err(internal)
}

fn next_pending(conn: &Connection, ref_name: &str) -> Result<Option<DeployRow>> {
    let sql = format!(
        "{SELECT_ROW} WHERE ref_name=?1 AND state='pending' \
         ORDER BY green_seq ASC, step_index ASC, id ASC LIMIT 1"
    );
    conn.query_row(&sql, [ref_name], read_row)
        .optional()
        .map_err(internal)
}

fn read_row(r: &rusqlite::Row) -> rusqlite::Result<DeployRow> {
    let commit_hex: String = r.get(3)?;
    let from_hex: Option<String> = r.get(4)?;
    let commit = commit_hex.parse().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let from = from_hex
        .map(|hex| {
            hex.parse().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    Ok(DeployRow {
        id: r.get(0)?,
        ref_name: r.get(1)?,
        step: r.get(2)?,
        commit,
        from,
        command: r.get(5)?,
        paths: r.get(6)?,
        secrets: r.get(7)?,
        platforms: r.get(8)?,
        state: r.get(9)?,
        superseded_by: r.get(10)?,
        output: r.get(11)?,
        submitter: r.get(12)?,
        submitted_at: r.get(13)?,
    })
}

fn due_detail(row: &DeployRow) -> Result<serde_json::Value> {
    let paths: serde_json::Value = serde_json::from_str(&row.paths).map_err(internal)?;
    let secrets: serde_json::Value = serde_json::from_str(&row.secrets).map_err(internal)?;
    let platforms: serde_json::Value = serde_json::from_str(&row.platforms).map_err(internal)?;
    Ok(serde_json::json!({
        "id": row.id,
        "step": row.step,
        "commit": row.commit,
        "from": row.from,
        "command": row.command,
        "paths": paths,
        "secrets": secrets,
        "platforms": platforms,
    }))
}

fn result_detail(record: &DeployRecord) -> serde_json::Value {
    serde_json::json!({
        "id": record.id,
        "step": record.step,
        "commit": record.commit,
        "status": record.status,
        "output": record.output,
        "submitter": record.submitter,
        "at": record.submitted_at,
    })
}
