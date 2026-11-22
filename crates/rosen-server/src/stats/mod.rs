//! Timing persistence and stats queries (SPEC R8, R9, R10, R12, #449).

pub mod query;
pub mod store;

pub use store::ensure_schema;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rosen_protocol::{
    ErrorBody, ErrorCode, Outcome, RecordPostLandTiming, RecordRunTiming, Response, StatsQuery,
};

use crate::ServerState;

/// Tracks mutations to timing tables so the Collapse state feed notices new
/// run-queue, checkpoint or land timings without polling SQLite.
#[derive(Default)]
pub struct Tracker {
    version: AtomicU64,
}

impl Tracker {
    pub fn bump(&self) {
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Answers [`rosen_protocol::Request::Stats`] on the blocking pool.
pub async fn handle_query(state: &Arc<ServerState>, query: StatsQuery) -> Outcome {
    let state = Arc::clone(state);
    let work = move || {
        let path = state.config.metadata_path();
        if !path.exists() {
            return query::empty_report(query.since_ms, now_millis());
        }
        let conn = crate::metadata::open(&path).map_err(internal)?;
        query::compute(&conn, &query, now_millis())
    };
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(report)) => Outcome::Ok(Response::Stats(Box::new(report))),
        Ok(Err(error)) => Outcome::Error(error),
        Err(join) => Outcome::Error(internal(join)),
    }
}

/// Answers [`rosen_protocol::Request::RecordRunTiming`] on the blocking pool.
pub async fn handle_record_run(
    state: &Arc<ServerState>,
    record: RecordRunTiming,
    machine: String,
) -> Outcome {
    if record.label.trim().is_empty() {
        return Outcome::Error(ErrorBody::new(
            ErrorCode::InvalidArgument,
            "run timing label must not be empty",
        ));
    }
    let state = Arc::clone(state);
    let work = move || {
        std::fs::create_dir_all(&state.config.data_dir).map_err(internal)?;
        let conn = crate::metadata::open(&state.config.metadata_path()).map_err(internal)?;
        let change = record
            .change
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .and_then(|c| {
                crate::changes::resolve(&conn, c)
                    .ok()
                    .map(|id| id.to_string())
            });
        store::record_run(&conn, &record, change.as_deref(), &machine).map_err(internal)?;
        state.stats.bump();
        Ok::<_, ErrorBody>(Response::RunTimingRecorded)
    };
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(error)) => Outcome::Error(error),
        Err(join) => Outcome::Error(internal(join)),
    }
}

/// Answers [`rosen_protocol::Request::RecordPostLandTiming`] on the blocking pool.
pub async fn handle_record_post_land(
    state: &Arc<ServerState>,
    record: RecordPostLandTiming,
) -> Outcome {
    let state = Arc::clone(state);
    let work = move || {
        std::fs::create_dir_all(&state.config.data_dir).map_err(internal)?;
        let conn = crate::metadata::open(&state.config.metadata_path()).map_err(internal)?;
        store::record_post_land_durations(&conn, &record).map_err(internal)?;
        state.stats.bump();
        Ok::<_, ErrorBody>(Response::RunTimingRecorded)
    };
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(error)) => Outcome::Error(error),
        Err(join) => Outcome::Error(internal(join)),
    }
}

pub(crate) fn internal(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, error.to_string())
}
