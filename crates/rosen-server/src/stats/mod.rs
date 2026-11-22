//! Timing persistence and stats queries (SPEC R8, R9, R10, R12, #449).

pub mod metrics;
pub mod query;
pub mod store;

pub use store::ensure_schema;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rosen_core::ObjectId;
use rosen_protocol::{
    CheckDurations, CheckKind, ErrorBody, ErrorCode, Outcome, PostLandJob, RecordPostLandTiming,
    RecordRunTiming, Response, StatsQuery,
};

use crate::ServerState;

type PostLandOfferKey = (CheckKind, String, ObjectId);

const MAX_TRACKED_OFFERS: usize = 256;

/// Tracks mutations to timing tables so the Collapse state feed notices new
/// run-queue, checkpoint or land timings without polling SQLite, and remembers
/// recent post-land / bisection offer timestamps so external checkers that
/// omit explicit durations still record elapsed check durations (#449, #464).
#[derive(Default)]
pub struct Tracker {
    version: AtomicU64,
    post_land_offers: Mutex<HashMap<PostLandOfferKey, i64>>,
    bisect_offers: Mutex<HashMap<(i64, u64), i64>>,
}

impl Tracker {
    pub fn bump(&self) {
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Records when a post-land batch job was offered to an external checker.
    pub fn record_post_land_offer(
        &self,
        kind: CheckKind,
        platform: Option<&str>,
        commit: ObjectId,
        at_ms: i64,
    ) {
        let mut offers = self
            .post_land_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if offers.len() >= MAX_TRACKED_OFFERS {
            offers.clear();
        }
        offers.insert((kind, platform.unwrap_or("").to_owned(), commit), at_ms);
    }

    /// Consumes the recorded offer time for `(kind, platform, commit)`. When
    /// an external checker (such as Velocity's `merge_build` pass) submits a
    /// companion build job alongside an offered test job for the same commit
    /// and platform, peeks the companion kind's offer time without consuming it.
    pub fn take_post_land_offer(
        &self,
        kind: CheckKind,
        platform: Option<&str>,
        commit: ObjectId,
    ) -> Option<i64> {
        let mut offers = self
            .post_land_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let platform = platform.unwrap_or("").to_owned();
        if let Some(at) = offers.remove(&(kind, platform.clone(), commit)) {
            return Some(at);
        }
        let companion = match kind {
            CheckKind::Build => CheckKind::Test,
            CheckKind::Test => CheckKind::Build,
        };
        offers.get(&(companion, platform, commit)).copied()
    }

    /// Records when a bisection step was offered via `bisect_next` or advance.
    pub fn record_bisect_offer(&self, session: i64, step: u64, at_ms: i64) {
        let mut offers = self
            .bisect_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if offers.len() >= MAX_TRACKED_OFFERS {
            offers.clear();
        }
        offers.insert((session, step), at_ms);
    }

    /// Consumes the recorded offer time for `(session, step)`.
    pub fn take_bisect_offer(&self, session: i64, step: u64) -> Option<i64> {
        self.bisect_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(session, step))
    }
}

/// Computes `(started_at_ms, durations_with_estimated_flag)` for an accepted
/// `PostLandJob`. Explicit `job.durations` always wins; otherwise non-empty
/// check sets fall back to offer-to-submit elapsed wall-clock time (#464).
pub fn resolve_job_timing(
    job: &PostLandJob,
    offered_fallback: Option<i64>,
    finished_at_ms: i64,
) -> (i64, Option<(CheckDurations, bool)>) {
    let offered_at = job.offered_at_ms.or(offered_fallback);
    if let Some(durations) = job.durations.as_deref().filter(|d| !d.is_empty()) {
        let started_at = offered_at
            .map(|at| at.min(finished_at_ms))
            .unwrap_or_else(|| {
                let total = i64::try_from(durations.total_ms().unwrap_or(0)).unwrap_or(i64::MAX);
                finished_at_ms.saturating_sub(total)
            });
        return (started_at, Some((durations.clone(), false)));
    }
    if !job.check_set.commands.is_empty() {
        if let Some(offered_at) = offered_at {
            let started_at = offered_at.min(finished_at_ms);
            let elapsed_ms = u64::try_from(finished_at_ms.saturating_sub(started_at)).unwrap_or(0);
            let durations =
                CheckDurations::from_elapsed(job.kind, &job.check_set.commands, elapsed_ms);
            return (started_at, Some((durations, true)));
        }
    }
    (finished_at_ms, None)
}

/// Computes `(durations, estimated)` for an accepted bisection step (#464).
pub fn resolve_bisect_step_durations(
    kind: CheckKind,
    explicit: Option<&CheckDurations>,
    offered_at: Option<i64>,
    finished_at_ms: i64,
) -> Option<(CheckDurations, bool)> {
    if let Some(durations) = explicit.filter(|d| !d.is_empty()) {
        return Some((durations.clone(), false));
    }
    let offered_at = offered_at?;
    let started_at = offered_at.min(finished_at_ms);
    let elapsed_ms = u64::try_from(finished_at_ms.saturating_sub(started_at)).unwrap_or(0);
    Some((CheckDurations::from_elapsed(kind, &[], elapsed_ms), true))
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
