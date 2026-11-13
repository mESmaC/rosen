//! The run's post-land checker (R12). Its full test fails any landed tree
//! holding the injected failure, so the server bisects to the culprit and
//! reverts it while the agents keep landing (§5).

use crate::machine::connect_when_up;
use crate::plan::Plan;
use rosen_client::protocol::{ReadAnswer, ReadRequest};
use rosen_client::{Client, Endpoint, Error, ObjectId};
use rosen_faults::checker::{step, CheckerLog, Step};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Longest one check may take before the checker assumes a fault stranded
/// it and reconnects.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone, Serialize)]
pub struct CheckerReport {
    #[serde(flatten)]
    pub log: CheckerLog,
    /// Checks a fault interrupted, after which the checker reconnected.
    pub interrupted: u64,
    /// Every landed commit was checked before the deadline.
    pub caught_up: bool,
}

/// When the checker stops.
pub struct Until<'a> {
    /// Set once every agent has finished landing.
    pub agents_done: &'a AtomicBool,
    /// Without an agents-done signal, as on a machine of a multi-machine
    /// run: stop once everything is checked and the base has stayed so
    /// this long.
    pub quiet: Option<Duration>,
    /// How long the checker may take to catch up after the agents finish.
    pub settle: Duration,
}

/// Checks landed commits until the agents are done and every landed
/// commit, the server's revert included, is green.
pub async fn run_checker(endpoint: &Endpoint, plan: &Plan, until: Until<'_>) -> CheckerReport {
    let mut report = CheckerReport::default();
    let mut client: Option<Client> = None;
    let mut give_up: Option<Instant> = None;
    let mut idle_since: Option<Instant> = None;
    loop {
        let quiet = match (until.quiet, idle_since) {
            (Some(quiet), Some(since)) => since.elapsed() >= quiet,
            _ => false,
        };
        // Read before the step, so an idle answer counts only when no
        // agent could land after it.
        let done = until.agents_done.load(Ordering::SeqCst) || quiet;
        if done && Instant::now() >= *give_up.get_or_insert_with(|| Instant::now() + until.settle) {
            return report;
        }
        let Some(connected) = &client else {
            client = connect_when_up(endpoint, Duration::from_secs(5)).await.ok();
            continue;
        };
        let test = |commit| holds_failure(connected, plan, commit);
        let checked = tokio::time::timeout(STEP_TIMEOUT, step(connected, &test, &mut report.log));
        match checked.await {
            Ok(Ok(Step::Idle { .. })) if done => {
                report.caught_up = true;
                return report;
            }
            Ok(Ok(Step::Idle { .. })) => {
                idle_since.get_or_insert_with(Instant::now);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Ok(Ok(Step::Worked)) => idle_since = None,
            Ok(Err(_)) | Err(_) => {
                report.interrupted += 1;
                idle_since = None;
                client = None;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// The run's full test: a landed tree fails when it holds the injected
/// failure.
async fn holds_failure(
    client: &Client,
    plan: &Plan,
    commit: ObjectId,
) -> anyhow::Result<Option<String>> {
    let Some(path) = plan.failure_path() else {
        return Ok(None);
    };
    let request = ReadRequest {
        at: Some(commit.to_string()),
        path: path.clone(),
    };
    match client.read_file(request).await {
        Ok(ReadAnswer::File { .. }) => Ok(Some(format!("{path} is present: injected failure"))),
        Ok(ReadAnswer::Directory { .. }) => Ok(None),
        Err(Error::Server(body)) if body.code == rosen_client::protocol::ErrorCode::NotFound => {
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}
