//! `rosen-faults drill`: a faulted run on one computer.
//!
//! Each simulated machine is a data directory and a local endpoint. Machine
//! 0 runs the server; every other machine runs a cache daemon that reaches
//! the server through its own [`PartitionProxy`](crate::proxy::PartitionProxy).
//! Agents are separate processes. A seeded plan kills and restarts all of
//! them and cuts machines off, then the drill checks §5's guarantees. The
//! full multi-machine stress harness drives the same plan through
//! [`crate::execute::Faults`].

pub mod agent;
mod enroll;
mod harness;
mod setup;
mod verify;

use crate::execute::execute;
use crate::plan::{Plan, Shape};
use crate::rng::Rng;
use harness::{Harness, Spec};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, clap::Args)]
pub struct DrillArgs {
    /// Replays a run exactly; printed by every run.
    #[arg(long)]
    pub seed: Option<u64>,
    /// Simulated machines, the server's included.
    #[arg(long, default_value_t = 3)]
    pub machines: usize,
    #[arg(long, default_value_t = 3)]
    pub agents_per_machine: usize,
    /// Changes each agent lands.
    #[arg(long, default_value_t = 6)]
    pub lands: usize,
    /// Seconds during which faults start.
    #[arg(long, default_value_t = 30)]
    pub window: u64,
    /// Seconds the run may take after the window before it counts as hung.
    #[arg(long, default_value_t = 180)]
    pub settle: u64,
    /// Inject a failing commit and check it is bisected and reverted (R12).
    #[arg(long)]
    pub failing_commit: bool,
    /// Defaults to the `rosen-server` next to this binary.
    #[arg(long)]
    pub server_bin: Option<PathBuf>,
    /// Where machines keep their data. Defaults to a new temp directory,
    /// kept after a failed run for inspection.
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

impl DrillArgs {
    fn shape(&self) -> Shape {
        let secs = Duration::from_secs;
        Shape {
            machines: self.machines.max(1),
            clients_per_machine: self.agents_per_machine,
            window: secs(self.window),
            server_kill_every: secs(15),
            daemon_kill_every: secs(12),
            client_kill_every: secs(10),
            down: (Duration::from_millis(200), secs(3)),
            partition_every: secs(10),
            partition_length: (secs(2), secs(8)),
        }
    }
}

/// Runs the drill and prints its report. True when every check passed.
pub async fn run(args: DrillArgs) -> anyhow::Result<bool> {
    let seed = args.seed.unwrap_or_else(Rng::fresh_seed);
    let shape = args.shape();
    let plan = Plan::draw(seed, &shape);
    let spec = spec(&args, seed)?;
    let units: Vec<String> = (0..shape.machines)
        .flat_map(|m| (0..args.agents_per_machine).map(move |i| setup::unit(m, i)))
        .collect();
    setup::create_project(&spec.layout, &units)?;
    eprintln!(
        "drill seed {seed}: {} kills, {} partitions, data in {}",
        plan.kills(),
        plan.partitions(),
        spec.layout.root.display()
    );
    let mut harness = Harness::start(&spec).await?;
    let start = tokio::time::Instant::now();
    let log = execute(&plan, &mut harness, start).await;
    let settled = harness.wait_for_agents(Duration::from_secs(args.settle)).await;
    let elapsed = start.elapsed();
    let crashed = harness.crashed();
    harness.stop().await;
    for problem in harness.forget_machines(&spec.layout) {
        eprintln!("drill: could not remove a machine credential: {problem}");
    }
    let acks = harness.acks.lock().expect("acks lock").clone();
    let checks = verify::verify(&spec.layout.server_config(), &acks, args.failing_commit)?;
    let ok = settled.is_ok() && crashed.is_empty() && checks.iter().all(|check| check.ok);
    let report = json!({
        "seed": seed,
        "ok": ok,
        "elapsed_ms": elapsed.as_millis(),
        "server_listen": harness.listen().to_string(),
        "settled": settled.as_ref().map(|_| "every agent finished".to_owned()).map_err(|err| format!("{err:#}")),
        "crashed": crashed,
        "checks": checks,
        "processes": harness.counts(),
        "faults": log,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    if ok && args.dir.is_none() {
        let _ = std::fs::remove_dir_all(&spec.layout.root);
    }
    Ok(ok)
}

fn spec(args: &DrillArgs, seed: u64) -> anyhow::Result<Spec> {
    let tag = format!("{:x}{:x}", std::process::id(), seed & 0xffff);
    let root = match &args.dir {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir().join(format!("rosen-faults-{tag}")),
    };
    let machines = args.machines.max(1);
    // The culprit is the first agent of the last machine, so its land and
    // its reopened change cross the network when there is one.
    let culprit = args
        .failing_commit
        .then_some((machines - 1, 0, args.lands / 2));
    Ok(Spec {
        layout: setup::Layout { root, tag },
        server_bin: setup::server_binary(args.server_bin.clone())?,
        machines,
        agents_per_machine: args.agents_per_machine.max(1),
        lands: args.lands,
        culprit,
    })
}
