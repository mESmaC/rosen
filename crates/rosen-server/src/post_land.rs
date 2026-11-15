//! Typed batch verification, separate from R8's post-land housekeeping.
mod builds;
pub mod failures;
pub mod store;

use crate::{landing::Ledger, ServerState};
use rosen_protocol::{
    CheckKind, ErrorBody, ErrorCode, EventKind, Outcome, PostLandJob, PostLandWork, Response,
};
use rusqlite::Connection;
use std::sync::{Arc, Mutex, PoisonError};

pub async fn handle(
    state: &Arc<ServerState>,
    passed: Option<PostLandJob>,
    kind: CheckKind,
) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || run(&state, passed, kind)).await {
        Ok(Ok(work)) => Outcome::Ok(Response::PostLand(work)),
        Ok(Err(err)) => Outcome::Error(err),
        Err(err) => Outcome::Error(internal(err)),
    }
}

fn open(state: &ServerState) -> Result<Connection, ErrorBody> {
    std::fs::create_dir_all(&state.config.data_dir).map_err(internal)?;
    let conn = crate::metadata::open(&state.config.metadata_path()).map_err(internal)?;
    store::ensure_schema(&conn).map_err(internal)?;
    builds::ensure_schema(&conn).map_err(internal)?;
    failures::ensure_schema(&conn).map_err(internal)?;
    Ok(conn)
}

fn run(
    state: &ServerState,
    passed: Option<PostLandJob>,
    kind: CheckKind,
) -> Result<PostLandWork, ErrorBody> {
    let mut conn = open(state)?;
    let ledger = Ledger::open(&state.config.metadata_path()).map_err(internal)?;
    let Some((name, _)) = ledger.project_base().map_err(internal)? else {
        return Ok(PostLandWork {
            job: None,
            last_green: None,
        });
    };
    if let Some(job) = passed {
        validate(state, &ledger, &name, &job)?;
        if !passed_job(&mut conn, &name, &job).map_err(internal)? {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgument,
                "commit is not a land",
            ));
        }
    }
    deliver(state, &conn)?;
    let job = newest(&conn, &name, kind)?
        .map(|commit| job_at(state, commit, kind))
        .transpose()?;
    Ok(PostLandWork {
        job,
        last_green: last_green(&conn, &name, kind)?,
    })
}

fn job_at(
    state: &ServerState,
    commit: rosen_core::ObjectId,
    kind: CheckKind,
) -> Result<PostLandJob, ErrorBody> {
    let config = state.project_config.at_commit(&commit).map_err(internal)?;
    Ok(PostLandJob {
        commit,
        kind,
        check_set: match kind {
            CheckKind::Test => config.config.checks.post_land.clone(),
            CheckKind::Build => config.config.checks.pre_land.clone(),
        },
    })
}

/// Validates the immutable batch binding, shared with failure submission.
pub fn validate(
    state: &ServerState,
    ledger: &Ledger,
    name: &str,
    job: &PostLandJob,
) -> Result<(), ErrorBody> {
    if ledger
        .base_seq(name, &job.commit)
        .map_err(internal)?
        .is_none()
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "commit was never on the base",
        ));
    }
    if job_at(state, job.commit, job.kind)? != *job {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "wrong post-land check set",
        ));
    }
    Ok(())
}

/// Crash replay may duplicate a notification, as R9 permits, but cannot lose it.
pub fn recover(state: &ServerState) -> Result<(), ErrorBody> {
    deliver(state, &open(state)?)
}

/// Serialises delivery. Each pass reads the pending rows and deletes a row
/// only after publishing it, so two concurrent passes (two checker requests,
/// or startup recovery racing one) would both publish every row (#149).
static DELIVERY: Mutex<()> = Mutex::new(());

fn deliver(state: &ServerState, conn: &Connection) -> Result<(), ErrorBody> {
    // A pass that panicked left no partial state the next pass cannot redo.
    let _turn = DELIVERY.lock().unwrap_or_else(PoisonError::into_inner);
    for (seq, change, commit) in store::pending(conn).map_err(internal)? {
        let published = state.events.publish(
            EventKind::Green,
            Some(change),
            serde_json::json!({ "commit": commit }),
        );
        if published == 0 {
            break;
        }
        conn.execute("DELETE FROM post_land_delivery WHERE seq=?1", [seq])
            .map_err(internal)?;
    }
    Ok(())
}

fn internal(err: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, err.to_string())
}

fn passed_job(conn: &mut Connection, name: &str, job: &PostLandJob) -> rusqlite::Result<bool> {
    match job.kind {
        CheckKind::Test => store::passed(conn, name, &job.commit),
        CheckKind::Build => builds::passed(conn, name, &job.commit),
    }
}

pub fn last_green(
    conn: &Connection,
    name: &str,
    kind: CheckKind,
) -> Result<Option<rosen_core::ObjectId>, ErrorBody> {
    match kind {
        CheckKind::Test => store::last_green(conn, name),
        CheckKind::Build => {
            builds::ensure_schema(conn).map_err(internal)?;
            builds::last_green(conn, name)
        }
    }
    .map_err(internal)
}

fn newest(
    conn: &Connection,
    name: &str,
    kind: CheckKind,
) -> Result<Option<rosen_core::ObjectId>, ErrorBody> {
    match kind {
        CheckKind::Test => store::newest(conn, name),
        CheckKind::Build => builds::newest(conn, name),
    }
    .map_err(internal)
}
