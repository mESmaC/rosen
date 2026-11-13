//! The external post-land checker (R12) a stress run needs, so an injected
//! failing commit is bisected and reverted (SPEC.md §5).
//!
//! A real checker runs the project's full tests; a run passes in its own
//! [`PostLandTest`], such as [`InjectedFailure`]. One [`step`] tests the
//! newest untested landed commit and, when it fails, answers every
//! bisection job until the server names a culprit, which the server then
//! reverts itself. [`run`] repeats steps through faults until every landed
//! commit is tested.

use rosen_client::{Client, Endpoint, Error};
use rosen_core::ObjectId;
use rosen_protocol::{
    BisectCulprit, BisectResult, BisectState, ErrorCode, ReadAnswer, ReadRequest,
};
use serde::Serialize;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Longest one check may take before the checker assumes a fault stranded
/// it and reconnects. Longer than any planned partition, so a step is only
/// abandoned when it can no longer finish.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// What the checker did, for a run's report.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CheckerLog {
    /// Batches whose full tests passed, each marking a prefix green.
    pub passed: u64,
    /// Batches that failed and started a bisection.
    pub failed: u64,
    /// Bisection jobs answered, confirmations included.
    pub bisect_steps: u64,
    /// Culprits the server confirmed, and so reverted.
    pub culprits: Vec<BisectCulprit>,
    /// Bisections stopped on contradictory results.
    pub flaky: u64,
}

/// What one [`step`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Every landed commit up to the base tip is tested.
    Idle { last_green: Option<ObjectId> },
    /// A batch was tested, and bisected if it failed.
    Worked,
}

/// Runs `test` on the newest untested landed commit and reports the result.
/// `test` answers `None` when the commit passes and the failure output when
/// it fails.
pub async fn step<F, Fut>(client: &Client, test: &F, log: &mut CheckerLog) -> anyhow::Result<Step>
where
    F: Fn(ObjectId) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<String>>>,
{
    let work = client.post_land_next().await?;
    let Some(job) = work.job else {
        return Ok(Step::Idle {
            last_green: work.last_green,
        });
    };
    match test(job.commit).await? {
        None => {
            client.post_land_passed(job).await?;
            log.passed += 1;
        }
        Some(output) => {
            log.failed += 1;
            let state = client.post_land_failed(job, output).await?;
            bisect(client, test, state, log).await?;
        }
    }
    Ok(Step::Worked)
}

/// Answers the session's jobs until it names a culprit or stops.
async fn bisect<F, Fut>(
    client: &Client,
    test: &F,
    mut state: BisectState,
    log: &mut CheckerLog,
) -> anyhow::Result<()>
where
    F: Fn(ObjectId) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<String>>>,
{
    while let Some(job) = state.job.clone() {
        let failed = test(job.commit).await?;
        log.bisect_steps += 1;
        let result = BisectResult {
            job,
            passed: failed.is_none(),
            output: failed.unwrap_or_default(),
        };
        state = client.bisect_submit(result).await?;
    }
    if let Some(culprit) = state.culprit {
        if !log
            .culprits
            .iter()
            .any(|seen| seen.commit == culprit.commit)
        {
            log.culprits.push(culprit);
        }
    }
    log.flaky += u64::from(state.flaky);
    Ok(())
}

/// A run's full test of one landed commit.
pub trait PostLandTest {
    /// `None` when `commit` passes, the failure output when it fails.
    fn test(
        &self,
        client: &Client,
        commit: ObjectId,
    ) -> impl Future<Output = anyhow::Result<Option<String>>>;
}

/// The stress runs' full test: a landed tree fails when it holds the
/// injected failure at `path`. With no path every commit passes.
pub struct InjectedFailure {
    pub path: Option<String>,
}

impl PostLandTest for InjectedFailure {
    async fn test(&self, client: &Client, commit: ObjectId) -> anyhow::Result<Option<String>> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let request = ReadRequest {
            at: Some(commit.to_string()),
            path: path.clone(),
        };
        match client.read_file(request).await {
            Ok(ReadAnswer::File { .. }) => Ok(Some(format!("{path} is present: injected failure"))),
            Ok(ReadAnswer::Directory { .. }) => Ok(None),
            Err(Error::Server(body)) if body.code == ErrorCode::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

/// What a checker [`run`] did, for a run's report.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CheckerReport {
    #[serde(flatten)]
    pub log: CheckerLog,
    /// Checks a fault interrupted, after which the checker reconnected.
    pub interrupted: u64,
    /// Every landed commit was checked before the deadline.
    pub caught_up: bool,
}

/// When a checker [`run`] stops.
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
/// commit, the server's revert included, is tested. A fault that cuts the
/// checker off mid-bisection costs nothing: the server keeps the session,
/// and the next failure of the same commit resumes it.
pub async fn run(endpoint: &Endpoint, test: &impl PostLandTest, until: Until<'_>) -> CheckerReport {
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
            client = connect(endpoint).await;
            continue;
        };
        let check = |commit| test.test(connected, commit);
        let checked = tokio::time::timeout(STEP_TIMEOUT, step(connected, &check, &mut report.log));
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

/// One connection attempt that the server answers; `None` while it is
/// down or unreachable, so the caller retries.
async fn connect(endpoint: &Endpoint) -> Option<Client> {
    let attempt = tokio::time::timeout(Duration::from_secs(10), Client::connect(endpoint)).await;
    match attempt {
        Ok(Ok(client)) if client.ping().await.is_ok() => Some(client),
        _ => {
            tokio::time::sleep(Duration::from_millis(250)).await;
            None
        }
    }
}
