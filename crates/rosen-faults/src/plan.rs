//! A fault plan: which process dies when and for how long, and which
//! machine loses the server for which period (SPEC.md §5). The plan is a
//! pure function of its seed and shape, so a run that breaks a guarantee
//! replays with the same faults.

use crate::rng::Rng;
use serde::Serialize;
use std::time::Duration;

/// Machine 0 serves the project; machines `1..` reach it over the network.
pub type Machine = usize;

/// A process the plan can kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Target {
    /// The authoritative server, on machine 0.
    Server,
    /// The cache daemon on a non-server machine.
    Daemon { machine: Machine },
    /// One agent's client process.
    Client { machine: Machine, index: usize },
}

/// How a partitioned machine loses the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cut {
    /// Connections reset and new ones are refused, as when a route is gone.
    Refuse,
    /// Bytes stop flowing but nothing closes, as when packets are dropped.
    /// Only the daemon's own timeouts can notice, so this finds hangs.
    Silent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "fault", rename_all = "snake_case")]
pub enum Fault {
    /// Kill the process at once, with no chance to clean up, and start it
    /// again after `down`.
    Kill { target: Target, down: Duration },
    /// Make the server unreachable from `machine` for `length`.
    Partition {
        machine: Machine,
        cut: Cut,
        length: Duration,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scheduled {
    /// Time since the run started.
    pub at: Duration,
    #[serde(flatten)]
    pub fault: Fault,
}

/// The shape of a run the plan is drawn for.
#[derive(Debug, Clone)]
pub struct Shape {
    /// Machines, including the server's. At least 1.
    pub machines: usize,
    /// Agent client processes on each machine.
    pub clients_per_machine: usize,
    /// No fault starts after this, so the run can settle and be checked.
    pub window: Duration,
    /// Mean time between kills of the server.
    pub server_kill_every: Duration,
    /// Mean time between kills of each daemon.
    pub daemon_kill_every: Duration,
    /// Mean time between kills of each client.
    pub client_kill_every: Duration,
    /// How long a killed process stays down.
    pub down: (Duration, Duration),
    /// Mean time between partitions of each non-server machine.
    pub partition_every: Duration,
    /// How long a partition lasts.
    pub partition_length: (Duration, Duration),
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub seed: u64,
    pub faults: Vec<Scheduled>,
}

impl Plan {
    /// Draws a plan. Every role that exists is killed at least once, and
    /// every non-server machine is partitioned at least once, so even a short
    /// run covers each fault §5 names.
    pub fn draw(seed: u64, shape: &Shape) -> Self {
        let mut rng = Rng::new(seed);
        let mut faults = Vec::new();
        let kills = targets(shape).into_iter().map(|target| {
            let every = match target {
                Target::Server => shape.server_kill_every,
                Target::Daemon { .. } => shape.daemon_kill_every,
                Target::Client { .. } => shape.client_kill_every,
            };
            (target, every)
        });
        for (target, every) in kills {
            faults.extend(series(&mut rng, shape, every, shape.down, |down| {
                Fault::Kill { target, down }
            }));
        }
        for machine in 1..shape.machines {
            let lengths = shape.partition_length;
            let mut cut_rng = Rng::new(rng.next_u64());
            faults.extend(series(&mut rng, shape, shape.partition_every, lengths, |length| {
                let cut = if cut_rng.below(2) == 0 { Cut::Refuse } else { Cut::Silent };
                Fault::Partition { machine, cut, length }
            }));
        }
        faults.sort_by_key(|scheduled| scheduled.at);
        Self { seed, faults }
    }

    pub fn kills(&self) -> usize {
        self.faults
            .iter()
            .filter(|s| matches!(s.fault, Fault::Kill { .. }))
            .count()
    }

    pub fn partitions(&self) -> usize {
        self.faults.len() - self.kills()
    }
}

fn targets(shape: &Shape) -> Vec<Target> {
    let mut targets = vec![Target::Server];
    targets.extend((1..shape.machines).map(|machine| Target::Daemon { machine }));
    for machine in 0..shape.machines {
        targets.extend((0..shape.clients_per_machine).map(|index| Target::Client { machine, index }));
    }
    targets
}

/// One target's faults, in order. Each starts only after the previous one
/// has ended, so a process is never killed while it is already down and a
/// machine is never cut twice at once. The first is forced into the window.
fn series(
    rng: &mut Rng,
    shape: &Shape,
    every: Duration,
    lengths: (Duration, Duration),
    make: impl FnMut(Duration) -> Fault,
) -> Vec<Scheduled> {
    let mut make = make;
    let mut out = Vec::new();
    let anywhere = Duration::from_millis(rng.below(shape.window.as_millis().max(1) as u64));
    let mut at = anywhere.min(rng.exponential(every));
    while at < shape.window {
        let length = rng.between(lengths.0, lengths.1);
        out.push(Scheduled { at, fault: make(length) });
        at += length + rng.exponential(every);
    }
    out
}
