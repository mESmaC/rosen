//! The run's post-land checker (R12). Its full test fails any landed tree
//! holding the injected failure, so the server bisects to the culprit and
//! reverts it while the agents keep landing (§5). The loop itself is
//! `rosen_faults::checker`, shared with the fault drill.

use crate::plan::Plan;
use rosen_client::Endpoint;
use rosen_faults::checker::{run, InjectedFailure};
pub use rosen_faults::checker::{CheckerReport, Until};

/// Checks landed commits until the agents are done and every landed
/// commit, the server's revert included, is green.
pub async fn run_checker(endpoint: &Endpoint, plan: &Plan, until: Until<'_>) -> CheckerReport {
    let test = InjectedFailure {
        path: plan.failure_path(),
    };
    run(endpoint, &test, until).await
}
