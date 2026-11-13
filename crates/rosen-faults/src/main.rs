//! `rosen-faults`: faulted stress runs (SPEC.md §5).

use clap::{Parser, Subcommand};
use rosen_faults::drill::{self, agent};
use std::process::ExitCode;

/// Kills Rosen's server, daemons and clients at random points, cuts
/// machines off from the server, and checks that no work is lost.
#[derive(Debug, Parser)]
#[command(name = "rosen-faults", version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Runs a faulted drill on this computer and prints a JSON report.
    Drill(drill::DrillArgs),
    /// One drill agent. The drill starts these itself.
    #[command(hide = true)]
    Agent(agent::AgentArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    let result = match Args::parse().command {
        Command::Drill(args) => drill::run(args).await,
        Command::Agent(args) => agent::run(args).await.map(|()| true),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("rosen-faults: {err:#}");
            ExitCode::from(2)
        }
    }
}
