//! `rosen`: the Rosen command-line tool, a thin layer over `rosen-client`.

mod change_cmd;
mod claim_cmd;
mod commands;
mod import;
mod server;

use std::process::ExitCode;

use claim_cmd::ClaimVerb;
use clap::Parser;
use commands::{Cli, Command, ServerCommand};
use rosen_client::Endpoint;

/// Schema tag for `--json` error output. Bump it when the shape changes, so
/// agents parsing the output can detect the change (SPEC.md R10).
const ERROR_SCHEMA: &str = "rosen.error/v1";

fn main() -> ExitCode {
    let cli = Cli::parse();
    let operation = cli.command.path();
    match run(&cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            report_error(cli.json, operation, &error);
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> rosen_client::Result<server::Succeeded> {
    let endpoint = cli
        .endpoint
        .clone()
        .map_or_else(Endpoint::resolve, Endpoint::new);
    let json = cli.json;
    match &cli.command {
        Command::Server(ServerCommand::Start) => block_on(server::start(json, &endpoint)),
        Command::Server(ServerCommand::Status) => block_on(server::status(json, &endpoint)),
        Command::Events { after, agent, all } => {
            let agent = if *all {
                None
            } else {
                Some(change_cmd::resolve_owner(agent.as_deref())?)
            };
            block_on(server::stream_events(json, &endpoint, agent, *after))
        }
        Command::Change(change) => block_on(change_cmd::run(json, &endpoint, change)),
        Command::Claim(args) => block_on(claim_cmd::run(json, &endpoint, ClaimVerb::Claim, args)),
        Command::Share(args) => block_on(claim_cmd::run(json, &endpoint, ClaimVerb::Share, args)),
        Command::Wait(args) => block_on(claim_cmd::run(json, &endpoint, ClaimVerb::Wait, args)),
        Command::Release(args) => {
            block_on(claim_cmd::run(json, &endpoint, ClaimVerb::Release, args))
        }
        Command::Import {
            repo,
            branch,
            mirror,
        } => block_on(import::import(
            json,
            &endpoint,
            repo,
            branch.as_deref(),
            mirror.as_deref(),
        )),
        other => rosen_client::run_operation(other.path()).map(|()| true),
    }
}

fn block_on<T>(
    work: impl std::future::Future<Output = rosen_client::Result<T>>,
) -> rosen_client::Result<T> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(work)
}

fn report_error(json: bool, operation: &str, error: &rosen_client::Error) {
    if json {
        let body = serde_json::json!({
            "schema": ERROR_SCHEMA,
            "command": operation,
            "error": error.to_string(),
        });
        println!("{body}");
    } else {
        eprintln!("rosen: {error}");
    }
}
