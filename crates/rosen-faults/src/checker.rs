//! The external post-land checker (R12) a stress run needs, so an injected
//! failing commit is bisected and reverted (SPEC.md §5).
//!
//! A real checker runs the project's full tests; a run passes in its own
//! test, such as reading whether the injected failure is in the tree. One
//! [`step`] tests the newest untested landed commit and, when it fails,
//! answers every bisection job until the server names a culprit, which the
//! server then reverts itself.

use rosen_client::Client;
use rosen_core::ObjectId;
use rosen_protocol::{BisectCulprit, BisectResult, BisectState};
use serde::Serialize;
use std::future::Future;

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
        if !log.culprits.iter().any(|seen| seen.commit == culprit.commit) {
            log.culprits.push(culprit);
        }
    }
    log.flaky += u64::from(state.flaky);
    Ok(())
}
