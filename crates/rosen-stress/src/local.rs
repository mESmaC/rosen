//! `rosen-stress local`: the whole run on one computer. It seeds a
//! repository, starts a server and a cache daemon for every other
//! simulated machine (enrolled with one join key), runs each machine's
//! agents as their own process alongside the post-land checker, optionally
//! kills processes and cuts machines off on a seeded fault plan, and
//! verifies. Across real machines, `seed`, `agents`, `checker` and
//! `verify` do the same steps.

use crate::checker::{run_checker, CheckerReport, Until};
use crate::cluster::{Cluster, Layout};
use crate::machine::connect_when_up;
use crate::plan::Plan;
use crate::report::Report;
use crate::verify::{verify, Failures, Targets};
use anyhow::{bail, Context, Result};
use rosen_faults::execute::{execute, Logged};
use rosen_faults::plan::{Plan as FaultPlan, Shape};
use rosen_faults::process::Recipe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

pub struct LocalRun {
    /// A new directory for the seed, every machine's data, the
    /// workspaces, the logs and the reports.
    pub dir: PathBuf,
    /// Simulated machines, the server's included.
    pub machines: usize,
    /// Ready workspaces each machine's daemon keeps (R3).
    pub pool_size: usize,
    pub pool_wait: Duration,
    /// Kill and partition on a fault plan drawn from this seed.
    pub faults: Option<FaultArgs>,
    /// How long the agents may take before the run counts as hung.
    pub deadline: Duration,
}

#[derive(Debug, Clone, clap::Args)]
pub struct FaultArgs {
    /// Replays a faulted run exactly; printed by every faulted run.
    #[arg(long)]
    pub fault_seed: Option<u64>,
    /// Seconds from the start during which faults begin.
    #[arg(long, default_value_t = 120)]
    pub fault_window_secs: u64,
}

impl FaultArgs {
    fn shape(&self, machines: usize) -> Shape {
        let secs = Duration::from_secs;
        Shape {
            machines,
            // One `agents` process per machine holds all of its clients.
            clients_per_machine: 1,
            window: secs(self.fault_window_secs),
            server_kill_every: secs(40),
            daemon_kill_every: secs(35),
            client_kill_every: secs(45),
            down: (Duration::from_millis(200), secs(3)),
            partition_every: secs(30),
            partition_length: (secs(2), secs(8)),
        }
    }
}

/// What the run did beyond the machines' reports.
pub struct Outcome {
    pub reports: Vec<Report>,
    pub checker: CheckerReport,
    pub faults: Vec<Logged>,
    pub processes: Vec<(String, u32, u32)>,
    pub failures: Failures,
}

pub async fn run_local(plan: &Plan, local: &LocalRun) -> Result<Outcome> {
    if local.dir.exists() {
        bail!("{} already exists; give a new directory", local.dir.display());
    }
    std::fs::create_dir_all(&local.dir)?;
    let layout = Layout::new(&std::path::absolute(&local.dir)?, local.machines.max(1));
    let seed = layout.root.join("seed");
    crate::seed::write_seed(plan, &seed)?;
    let (mut cluster, listen) = Cluster::start_server(&layout).await?;
    let result = run_cluster(plan, local, &layout, &seed, &mut cluster, listen).await;
    cluster.stop(&layout).await;
    result
}

async fn run_cluster(
    plan: &Plan,
    local: &LocalRun,
    layout: &Layout,
    seed: &Path,
    cluster: &mut Cluster,
    listen: std::net::SocketAddr,
) -> Result<Outcome> {
    let server = connect_when_up(&layout.endpoint(0), Duration::from_secs(30)).await?;
    server
        .import(&seed.to_string_lossy(), Some("main"), None)
        .await
        .context("importing the seed")?;
    drop(server);
    cluster.start_daemons(layout, listen).await?;
    cluster.start_agents(agent_recipes(plan, local, layout)?)?;
    let agents_done = Arc::new(AtomicBool::new(false));
    let checker = tokio::spawn(checker_task(layout.endpoint(0), plan.clone(), Arc::clone(&agents_done)));
    let faults = match &local.faults {
        Some(args) => play_faults(args, layout.machines, cluster).await,
        None => Vec::new(),
    };
    let waited = cluster.wait_for_agents(local.deadline).await;
    agents_done.store(true, Ordering::SeqCst);
    let checker = checker.await.context("the checker task panicked")?;
    let mut failures = Failures::new();
    if let Err(err) = waited {
        failures.push(format!("agents: {err:#}"));
    }
    failures.extend(cluster.crashed().into_iter().map(|crash| format!("crash: {crash}")));
    let reports = read_reports(layout, &mut failures);
    let targets = Targets {
        latency: local.faults.is_none(),
        checker: Some(&checker),
    };
    let client = connect_when_up(&layout.endpoint(0), Duration::from_secs(30)).await?;
    failures.extend(verify(&client, &reports, &targets).await?);
    Ok(Outcome {
        reports,
        checker,
        faults,
        processes: cluster.counts(),
        failures,
    })
}

/// Plays a seeded fault plan through the cluster and returns its log.
async fn play_faults(args: &FaultArgs, machines: usize, cluster: &mut Cluster) -> Vec<Logged> {
    let seed = args.fault_seed.unwrap_or_else(rosen_faults::rng::Rng::fresh_seed);
    let plan = FaultPlan::draw(seed, &args.shape(machines));
    println!(
        "fault seed {seed}: {} kills, {} partitions in {} s",
        plan.kills(),
        plan.partitions(),
        args.fault_window_secs
    );
    execute(&plan, cluster, tokio::time::Instant::now()).await
}

/// The checker reaches the server directly, as a checker machine's daemon
/// would forward to it.
async fn checker_task(endpoint: rosen_client::Endpoint, plan: Plan, done: Arc<AtomicBool>) -> CheckerReport {
    let until = Until {
        agents_done: &done,
        quiet: None,
        settle: Duration::from_secs(300),
    };
    run_checker(&endpoint, &plan, until).await
}

/// One `rosen-stress agents` command per machine, splitting the agents
/// as evenly as the machine count allows.
fn agent_recipes(plan: &Plan, local: &LocalRun, layout: &Layout) -> Result<Vec<Recipe>> {
    let me = std::env::current_exe()?;
    let machines = layout.machines;
    let mut recipes: Vec<Recipe> = Vec::new();
    for machine in 0..machines {
        let first = plan.agents * machine / machines;
        let count = plan.agents * (machine + 1) / machines - first;
        let dir = layout.machine_dir(machine);
        let mut args: Vec<String> = vec![
            "agents".into(),
            "--machine".into(),
            Layout::machine_name(machine),
            "--first-agent".into(),
            first.to_string(),
            "--agent-count".into(),
            count.to_string(),
            "--endpoint".into(),
            layout.endpoint(machine).as_str().to_owned(),
            "--pool-size".into(),
            local.pool_size.to_string(),
            "--pool-wait-secs".into(),
            local.pool_wait.as_secs().to_string(),
        ];
        args.extend(plan.to_args());
        let paths = [
            ("--work-dir", dir.join("work")),
            ("--report", layout.report(machine)),
        ];
        let me = me.clone();
        recipes.push(Box::new(move || {
            let mut command = Command::new(&me);
            command.args(&args);
            for (flag, path) in &paths {
                command.arg(flag).arg(path);
            }
            command
        }));
    }
    Ok(recipes)
}

fn read_reports(layout: &Layout, failures: &mut Failures) -> Vec<Report> {
    let mut reports = Vec::new();
    for machine in 0..layout.machines {
        let path = layout.report(machine);
        let read = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?));
        match read {
            Ok(report) => reports.push(report),
            Err(err) => failures.push(format!("{}: {err:#}", path.display())),
        }
    }
    reports
}
