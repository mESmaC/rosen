//! Typed batch verification, separate from R8's post-land housekeeping.
pub(crate) mod builds;
pub mod deploy;
pub mod failures;
pub mod status;
pub mod store;
pub mod submitter;

use crate::{landing::Ledger, ServerState};
use rosen_protocol::{
    CheckKind, CheckPriority, ErrorBody, ErrorCode, EventKind, Outcome, PostLandJob, PostLandWork,
    Response,
};
use rusqlite::Connection;
use std::sync::{Arc, Mutex, PoisonError};

/// A passing run as the server accepted it, with who reported it.
pub type Passed = (PostLandJob, submitter::Submitted);

pub async fn handle(state: &Arc<ServerState>, passed: Option<Passed>, kind: CheckKind) -> Outcome {
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
    deploy::ensure_schema(&conn).map_err(internal)?;
    Ok(conn)
}

fn run(
    state: &ServerState,
    passed: Option<Passed>,
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
    if let Some((job, submitted)) = passed {
        validate(state, &ledger, &name, &job)?;
        if !passed_job(state, &mut conn, &name, &job, &submitted)? {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgument,
                "commit is not a land",
            ));
        }
    }
    deliver(state, &conn)?;
    state.events.deliver_staged();
    let kind = next_kind(state, &conn, &name, kind)?;
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
        priority: if kind == CheckKind::Build
            && config.config.recheck == rosen_core::RecheckMode::Merge
        {
            CheckPriority::Landing
        } else {
            CheckPriority::Verify
        },
        check_set: match kind {
            CheckKind::Test => config.config.checks.post_land.clone(),
            CheckKind::Build => config.config.checks.pre_land.clone(),
        },
        submitter: None,
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
    let expected = job_at(state, job.commit, job.kind)?;
    if expected.check_set != job.check_set {
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

/// Event detail published when a landed change is marked green (R9, R12).
pub(crate) fn green_detail(
    commit: &str,
    submitter: Option<&str>,
    at: Option<i64>,
) -> serde_json::Value {
    serde_json::json!({ "commit": commit, "submitter": submitter, "at": at })
}

fn deliver(state: &ServerState, conn: &Connection) -> Result<(), ErrorBody> {
    // A pass that panicked left no partial state the next pass cannot redo.
    let _turn = DELIVERY.lock().unwrap_or_else(PoisonError::into_inner);
    for row in store::pending(conn).map_err(internal)? {
        let published = state.events.publish(
            EventKind::Green,
            Some(row.change),
            green_detail(&row.commit, row.submitter.as_deref(), row.at),
        );
        if published == 0 {
            break;
        }
        conn.execute("DELETE FROM post_land_delivery WHERE seq=?1", [row.seq])
            .map_err(internal)?;
    }
    Ok(())
}

fn internal(err: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, err.to_string())
}

fn passed_job(
    state: &ServerState,
    conn: &mut Connection,
    name: &str,
    job: &PostLandJob,
    submitted: &submitter::Submitted,
) -> Result<bool, ErrorBody> {
    match job.kind {
        CheckKind::Test => {
            let resolved = state
                .project_config
                .at_commit(&job.commit)
                .map_err(internal)?;
            let objects = state.config.objects_dir();
            store::passed_by_with(conn, name, &job.commit, Some(submitted), |tx, prev, end| {
                deploy::on_green_range(
                    tx,
                    &objects,
                    name,
                    prev,
                    end,
                    &job.commit,
                    &resolved.config.deploy,
                )
            })
        }
        CheckKind::Build => {
            builds::passed(conn, name, &job.commit, Some(submitted)).map_err(internal)
        }
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

/// Merge-mode projects offer urgent build work before the full-test queue.
fn next_kind(
    state: &ServerState,
    conn: &Connection,
    name: &str,
    requested: CheckKind,
) -> Result<CheckKind, ErrorBody> {
    let merge = state
        .project_config
        .at_base()
        .map_err(internal)?
        .is_some_and(|(_, r)| r.config.recheck == rosen_core::RecheckMode::Merge);
    if requested == CheckKind::Test
        && merge
        && builds::newest(conn, name).map_err(internal)?.is_some()
    {
        Ok(CheckKind::Build)
    } else {
        Ok(requested)
    }
}

/// Keep new work off unbuilt tips. Until the first build passes, use the
/// imported starting point; it is not advertised as a verified green prefix.
pub(crate) fn workspace_base(
    config: &crate::ServerConfig,
    name: &str,
    resolved: &crate::project_config::Resolved,
) -> Result<rosen_core::ObjectId, String> {
    if resolved.config.recheck != rosen_core::RecheckMode::Merge {
        return Ok(resolved.commit);
    }
    let conn = crate::metadata::open(&config.metadata_path()).map_err(|e| e.to_string())?;
    builds::ensure_schema(&conn).map_err(|e| e.to_string())?;
    if let Some(built) = builds::last_green(&conn, name).map_err(|e| e.to_string())? {
        return Ok(built);
    }
    let initial: String = conn
        .query_row(
            "SELECT commit_id FROM base_history WHERE ref_name=?1 ORDER BY seq LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    initial
        .parse()
        .map_err(|e: rosen_core::Error| e.to_string())
}
