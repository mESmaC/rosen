//! `rosen-stress`: the SPEC.md §5 stress run.
//!
//! Agents create changes with local workspaces (some stacked), edit a
//! shared file and a file of their own, claim and share paths, race for a
//! contended path, receipt and approve, and land, over `rosen-client`
//! alone. The harness measures workspace start time and land latency, and
//! checks that no edit is lost, no land is applied twice, the base moves
//! only by the run's approved, receipted lands, and no path has two
//! claimants.
//!
//! Three machines, one serving the project, each enrolled with one join
//! key (R14):
//!
//! 1. On any machine: `rosen-stress seed --dir seed` with the run's plan
//!    flags, then on the server `rosen import seed`.
//! 2. On each machine, its slice of agents against its own daemon:
//!    `rosen-stress agents --machine m1 --first-agent 0 --agent-count 34
//!    --work-dir D:\ws --report m1.json` (plan flags identical everywhere).
//! 3. On any machine, with every report copied over:
//!    `rosen-stress verify m1.json m2.json m3.json`.
//!
//! `rosen-stress local --dir <new dir>` does all three on one machine with
//! a private server, for a quick run or CI.

mod agent;
mod claims;
mod edit;
mod local;
mod machine;
mod plan;
mod report;
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
    /// Merge machine reports, check §5's guarantees on the server, and
    /// print the measurements. Exits 1 when any guarantee is missed.
    Verify {
        #[arg(required = true)]
        reports: Vec<PathBuf>,
        /// The daemon to read the base through. Defaults to
        /// $ROSEN_ENDPOINT, then the per-user default.
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Seed, serve, run every agent and verify, all on this machine.
    Local {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, default_value_t = 100)]
        pool_size: usize,
        #[arg(long, default_value_t = 120)]
        pool_wait_secs: u64,
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
                pool_size: daemon.pool_size,
                pool_wait: Duration::from_secs(daemon.pool_wait_secs),
            };
            let result = run_machine(&plan, &run).await?;
            std::fs::write(&report, serde_json::to_vec_pretty(&result)?)?;
            print!("{}", verify::summary(std::slice::from_ref(&result)));
            Ok(true)
        }
        Command::Verify {
            reports,
            endpoint: named,
        } => verify_reports(&reports, endpoint(named)).await,
        Command::Local {
            dir,
            pool_size,
            pool_wait_secs,
            plan,
        } => {
            let local = local::LocalRun {
                dir,
                pool_size,
                pool_wait: Duration::from_secs(pool_wait_secs),
            };
            let (report, failures) = local::run_local(&plan, &local).await?;
            Ok(print_outcome(std::slice::from_ref(&report), &failures))
        }
    }
}

async fn verify_reports(paths: &[PathBuf], endpoint: Endpoint) -> Result<bool> {
    let mut reports = Vec::new();
    for path in paths {
        let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
        reports.push(serde_json::from_slice(&bytes).with_context(|| path.display().to_string())?);
    }
    let client = Client::connect(&endpoint).await?;
    let failures = verify::verify(&client, &reports).await?;
    Ok(print_outcome(&reports, &failures))
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
