//! One machine's measurements, written as JSON so `verify` can merge the
//! reports of every machine in the run.

use crate::plan::Plan;
use rosen_client::{ChangeId, ObjectId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub plan: Option<Plan>,
    pub machine: String,
    /// The agent indexes this machine ran, `first..first + count`.
    pub first_agent: usize,
    pub agent_count: usize,
    /// The base tip when this machine's agents started. `verify` walks the
    /// base back to the oldest of these.
    pub base_at_start: Option<ObjectId>,
    pub elapsed_ms: u64,
    pub workspace_starts: Vec<StartSample>,
    pub lands: Vec<LandSample>,
    pub changes: Vec<ChangeOutcome>,
    /// Two agents seen holding one claimed path at once.
    pub claim_violations: Vec<String>,
    /// Hot-path claims granted and refused, to show contention happened.
    pub hot_claims_granted: u64,
    pub hot_claims_refused: u64,
    /// Land refusals by reason code, including ones the agent recovered
    /// from by re-receipting.
    pub land_refusals: BTreeMap<String, u64>,
    /// Workspaces of closed changes the daemon had not deleted when the
    /// run ended (R4 deletes them automatically).
    pub workspaces_kept: Vec<String>,
    /// Requests that failed and were retried or abandoned.
    pub errors: Vec<String>,
}

/// Creating a change and its workspace (R3), as one agent waited for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSample {
    pub ms: f64,
    /// `warm_pool`, `block_clone`, `copy` or `mixed`.
    pub method: String,
}

/// One land request that moved the base, with the receipt it was given
/// just before for the exact head and base (R8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandSample {
    pub change: ChangeId,
    pub ms: f64,
    /// Compare-and-swap attempts inside the server.
    pub attempts: u32,
    /// Land requests the agent repeated after a fresh receipt.
    pub rechecks: u32,
}

/// What became of one change, and the lines that must be in the final
/// base if it landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeOutcome {
    pub change: ChangeId,
    pub agent: String,
    pub stacked_on: Option<ChangeId>,
    /// `(path, line)`: the file must hold this exact line.
    pub expected: Vec<(String, String)>,
    /// The change's local workspace on its machine.
    pub workspace: PathBuf,
    pub landed: Option<ObjectId>,
    pub failure: Option<String>,
}

/// The nearest-rank percentile, so a reported p95 is a real sample.
pub fn percentile(samples: &[f64], fraction: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}
