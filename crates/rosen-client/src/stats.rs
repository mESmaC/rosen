//! Timing persistence and stats queries over the Rosen protocol (#449).

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{
    RecordPostLandTiming, RecordRunTiming, Request, Response, StatsQuery, StatsReport,
};

impl Client {
    /// Queries per-change timelines, post-land checkpoints and window aggregates.
    pub async fn stats(&self, query: StatsQuery) -> Result<StatsReport, Error> {
        match self.request(Request::Stats(query)).await? {
            Response::Stats(report) => Ok(*report),
            other => Err(unexpected(other)),
        }
    }

    /// Persists one run-queue timing sample (counts and durations only).
    pub async fn record_run_timing(&self, record: RecordRunTiming) -> Result<(), Error> {
        match self.request(Request::RecordRunTiming(record)).await? {
            Response::RunTimingRecorded => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Persists build/lint/test durations for a post-land checkpoint or bisection.
    pub async fn record_post_land_timing(&self, record: RecordPostLandTiming) -> Result<(), Error> {
        match self.request(Request::RecordPostLandTiming(record)).await? {
            Response::RunTimingRecorded => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}
