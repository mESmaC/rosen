//! Plays a [`Plan`] in real time against whatever runs the processes. A
//! harness implements [`Faults`] for its own process supervisor and
//! partition proxies; the drill in this crate is one such harness.

use crate::plan::{Cut, Fault, Machine, Plan, Target};
use serde::Serialize;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// The operations a plan needs from a harness.
pub trait Faults {
    /// Kills the process abruptly, as a crash or power loss would: on
    /// Windows, `TerminateProcess`, with no shutdown request first.
    fn kill(&mut self, target: Target) -> impl Future<Output = anyhow::Result<()>>;
    fn restart(&mut self, target: Target) -> impl Future<Output = anyhow::Result<()>>;
    fn cut(&mut self, machine: Machine, cut: Cut);
    fn heal(&mut self, machine: Machine);
}

/// One step a plan took, for the run's log.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Kill { target: Target },
    Restart { target: Target },
    Cut { machine: Machine, cut: Cut },
    Heal { machine: Machine },
}

#[derive(Debug, Clone, Serialize)]
pub struct Logged {
    pub at_ms: u128,
    #[serde(flatten)]
    pub action: Action,
    /// Set when the harness failed to carry the action out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Runs every fault in `plan` from `start`, including each fault's end, so
/// every killed process is running again and every machine is reconnected
/// when it returns.
pub async fn execute(plan: &Plan, faults: &mut impl Faults, start: Instant) -> Vec<Logged> {
    let mut log = Vec::new();
    for (at, action) in timeline(plan) {
        tokio::time::sleep_until(start + at).await;
        let error = apply(faults, &action).await.err().map(|err| format!("{err:#}"));
        log.push(Logged {
            at_ms: start.elapsed().as_millis(),
            action,
            error,
        });
    }
    log
}

/// Every start and end, in time order. The sort is stable and each fault's
/// end is pushed after its start, so ties keep a target's start first.
fn timeline(plan: &Plan) -> Vec<(Duration, Action)> {
    let mut steps = Vec::new();
    for scheduled in &plan.faults {
        match scheduled.fault {
            Fault::Kill { target, down } => {
                steps.push((scheduled.at, Action::Kill { target }));
                steps.push((scheduled.at + down, Action::Restart { target }));
            }
            Fault::Partition {
                machine,
                cut,
                length,
            } => {
                steps.push((scheduled.at, Action::Cut { machine, cut }));
                steps.push((scheduled.at + length, Action::Heal { machine }));
            }
        }
    }
    steps.sort_by_key(|(at, _)| *at);
    steps
}

async fn apply(faults: &mut impl Faults, action: &Action) -> anyhow::Result<()> {
    match *action {
        Action::Kill { target } => faults.kill(target).await,
        Action::Restart { target } => faults.restart(target).await,
        Action::Cut { machine, cut } => {
            faults.cut(machine, cut);
            Ok(())
        }
        Action::Heal { machine } => {
            faults.heal(machine);
            Ok(())
        }
    }
}
