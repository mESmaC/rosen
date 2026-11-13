//! `rosen-stress`: the SPEC.md §5 stress run.
//!
//! Agents create changes with local workspaces (some stacked), edit a
//! shared file and a file of their own, claim and share paths, race for a
//! contended path, receipt and approve, and land, over `rosen-client`
//! alone. One change carries an injected failure that a post-land checker
//! finds, so the server bisects and reverts it (R12). The harness measures
//! workspace start time and land latency, and checks that no edit is lost,
//! no land is applied twice, the base moves only by the run's approved,
//! receipted lands and the server's revert, the culprit reopens with its
//! work, and no path has two claimants.
//!
//! Three machines, one serving the project, each enrolled with one join
//! key (R14):
//!
//! 1. On any machine: `rosen-stress seed --dir seed` with the run's plan
//!    flags, then on the server `rosen import seed`.
//! 2. On each machine, its slice of agents against its own daemon:
//!    `rosen-stress agents --machine m1 --first-agent 0 --agent-count 34
//!    --work-dir D:\ws --report m1.json` (plan flags identical everywhere).
//!    Started again after a kill, it resumes from its journal.
//! 3. On one machine, alongside the agents: `rosen-stress checker`.
//! 4. On any machine, with every report copied over:
//!    `rosen-stress verify m1.json m2.json m3.json`.
//!
//! `rosen-stress local --dir <new dir>` does all of it on one computer,
//! with a server and a cache daemon per simulated machine; `--faults`
//! also kills processes and cuts machines off on a seeded plan.

mod agent;
mod checker;
mod claims;
mod cluster;
mod edit;
mod local;
mod machine;
mod plan;
mod report;
mod revert;
mod seed;
mod verify;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use machine::{run_machine, MachineRun};
use plan::Plan;
use rosen_client::{Client, Endpoint};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "rosen-stress", version, about = "The SPEC.md §5 stress run")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the git repository a stress project is imported from.
    Seed {
        #[arg(long)]
        dir: PathBuf,
        #[command(flatten)]
        plan: Plan,
    },
    /// Run this machine's agents against its daemon and write a report.
    /// Records go to `<report>.journal` as they happen; started again on
    /// the same report after a kill, the agents resume where they stopped.
    Agents {
        /// This machine's name in the report and in reviewer nicks.
        #[arg(long)]
        machine: String,
        #[arg(long, default_value_t = 0)]
        first_agent: usize,
        #[arg(long)]
        agent_count: usize,
        #[arg(long)]
        work_dir: PathBuf,
        #[arg(long)]
        report: PathBuf,
        #[command(flatten)]
        daemon: DaemonArgs,
        #[command(flatten)]
        plan: Plan,
    },
    /// Run the post-land checker (R12) until every landed commit is
    /// checked and the base has stayed still for `--quiet-secs`, then
    /// print what it did as JSON.
    Checker {
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long, default_value_t = 60)]
        quiet_secs: u64,
        #[command(flatten)]
        plan: Plan,
    },
    /// Merge machine reports, check §5's guarantees on the server, and
    /// print the measurements. Exits 1 when any guarantee is missed.
    Verify {
        #[arg(required = true)]
        reports: Vec<PathBuf>,
        /// The daemon to read the base through. Defaults to
        /// $ROSEN_ENDPOINT, then the per-user default.
        #[arg(long)]
        endpoint: Option<String>,
        /// Skip the p95 latency targets, as for a faulted run.
        #[arg(long)]
        no_latency: bool,
    },
    /// Seed, serve, run every machine's agents and the checker, and
    /// verify, all on this computer.
    Local {
        #[arg(long)]
        dir: PathBuf,
        /// Simulated machines, the server's included.
        #[arg(long, default_value_t = 3)]
        machines: usize,
        /// Ready workspaces each machine's daemon keeps.
        #[arg(long, default_value_t = 40)]
        pool_size: usize,
        #[arg(long, default_value_t = 180)]
        pool_wait_secs: u64,
        /// Seconds the agents may run before the run counts as hung.
        #[arg(long, default_value_t = 3600)]
        deadline_secs: u64,
        /// Kill the server, daemons and agents and cut machines off at
        /// random points (§5); the latency targets are then not applied.
        #[arg(long)]
        faults: bool,
        #[command(flatten)]
        fault: local::FaultArgs,
        #[command(flatten)]
        plan: Plan,
    },
}

#[derive(clap::Args)]
struct DaemonArgs {
    #[arg(long)]
    endpoint: Option<String>,
    /// Ready workspaces to ask this machine's daemon for before starting.
    #[arg(long, default_value_t = 40)]
    pool_size: usize,
    #[arg(long, default_value_t = 120)]
    pool_wait_secs: u64,
}

fn endpoint(named: Option<String>) -> Endpoint {
    named.map_or_else(Endpoint::resolve, Endpoint::new)
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse().command).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("rosen-stress: {err:#}");
            ExitCode::from(2)
        }
    }
}

/// Runs one command; `Ok(false)` means the run missed a guarantee.
async fn run(command: Command) -> Result<bool> {
    match command {
        Command::Seed { dir, plan } => seed::write_seed(&plan, &dir).map(|()| true),
        Command::Agents {
            machine,
            first_agent,
            agent_count,
            work_dir,
            report,
            daemon,
            plan,
        } => {
            let run = MachineRun {
                machine,
                first_agent,
                agent_count,
                endpoint: endpoint(daemon.endpoint),
                work_dir,
                journal: report.with_extension("journal"),
                pool_size: daemon.pool_size,
                pool_wait: Duration::from_secs(daemon.pool_wait_secs),
            };
            run_agents(&plan, &run, &report).await
        }
        Command::Checker {
            endpoint: named,
            quiet_secs,
            plan,
        } => run_checker(endpoint(named), quiet_secs, &plan).await,
        Command::Verify {
            reports,
            endpoint: named,
            no_latency,
        } => verify_reports(&reports, endpoint(named), !no_latency).await,
        Command::Local {
            dir,
            machines,
            pool_size,
            pool_wait_secs,
            deadline_secs,
            faults,
            fault,
            plan,
        } => {
            let local = local::LocalRun {
                dir,
                machines,
                pool_size,
                pool_wait: Duration::from_secs(pool_wait_secs),
                faults: faults.then_some(fault),
                deadline: Duration::from_secs(deadline_secs),
            };
            let outcome = local::run_local(&plan, &local).await?;
            Ok(print_local(&outcome))
        }
    }
}

async fn run_agents(plan: &Plan, run: &MachineRun, report: &std::path::Path) -> Result<bool> {
    let result = run_machine(plan, run).await?;
    std::fs::write(report, serde_json::to_vec_pretty(&result)?)?;
    print!("{}", verify::summary(std::slice::from_ref(&result)));
    Ok(true)
}

async fn run_checker(endpoint: Endpoint, quiet_secs: u64, plan: &Plan) -> Result<bool> {
    let never = std::sync::atomic::AtomicBool::new(false);
    let until = checker::Until {
        agents_done: &never,
        quiet: Some(Duration::from_secs(quiet_secs)),
        settle: Duration::from_secs(quiet_secs),
    };
    let report = checker::run_checker(&endpoint, plan, until).await;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(report.caught_up)
}

async fn verify_reports(paths: &[PathBuf], endpoint: Endpoint, latency: bool) -> Result<bool> {
    let mut reports = Vec::new();
    for path in paths {
        let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
        reports.push(serde_json::from_slice(&bytes).with_context(|| path.display().to_string())?);
    }
    let client = Client::connect(&endpoint).await?;
    let targets = verify::Targets {
        latency,
        checker: None,
    };
    let failures = verify::verify(&client, &reports, &targets).await?;
    Ok(print_outcome(&reports, &failures))
}

fn print_local(outcome: &local::Outcome) -> bool {
    let checker = &outcome.checker.log;
    println!(
        "checker: {} batches passed, {} failed, {} bisection steps, culprits {:?}, {} interrupted",
        checker.passed,
        checker.failed,
        checker.bisect_steps,
        checker.culprits.iter().map(|c| c.commit).collect::<Vec<_>>(),
        outcome.checker.interrupted
    );
    let faults = outcome.faults.iter().filter(|f| f.error.is_some()).count();
    println!("faults played {}, failed to apply {faults}", outcome.faults.len());
    for (name, kills, starts) in &outcome.processes {
        println!("  {name}: killed {kills}, started {starts}");
    }
    print_outcome(&outcome.reports, &outcome.failures)
}

fn print_outcome(reports: &[report::Report], failures: &verify::Failures) -> bool {
    print!("{}", verify::summary(reports));
    if failures.is_empty() {
        println!("PASS: every §5 guarantee held");
        return true;
    }
    println!("FAIL: {} guarantee(s) missed", failures.len());
    for failure in failures {
        println!("  {failure}");
    }
    false
}
