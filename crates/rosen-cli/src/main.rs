//! `rosen`: the Rosen command-line tool, a thin layer over `rosen-client`.

mod change_cmd;
mod claim_cmd;
mod commands;
mod commit_cmd;
mod config_cmd;
mod history_cmd;
mod import;
mod land_cmd;
mod machine_cmd;
mod overlaps_cmd;
mod receipt_cmd;
mod review_cmd;
mod server;
mod status_cmd;
mod workspace_cmd;

use std::process::ExitCode;

use claim_cmd::ClaimVerb;
use clap::Parser;
use commands::{Cli, Command, ServerCommand, WorkspaceCommand};
use rosen_client::Endpoint;

/// Schema tag for `--json` error output. Bump it when the shape changes, so
/// agents parsing the output can detect the change (SPEC.md R10).
const ERROR_SCHEMA: &str = "rosen.error/v1";

/// Exit status for an invalid command line, as clap uses.
const USAGE_EXIT: u8 = 2;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return report_usage_error(&error),
    };
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
    if let Some((verb, change, paths)) = path_record_call(&cli.command) {
        return block_on(claim_cmd::run(json, &endpoint, verb, change, paths));
    }
    match &cli.command {
        Command::Server(ServerCommand::Start) => block_on(server::start(json, &endpoint)),
        Command::Server(ServerCommand::Status) => block_on(server::status(json, &endpoint)),
        Command::Events { after, agent, all } => {
            events(json, &endpoint, *after, agent.as_deref(), *all)
        }
        Command::Config(config) => block_on(config_cmd::run(json, &endpoint, config)),
        Command::Machine(command) => block_on(machine_cmd::run(json, &endpoint, command)),
        Command::Change(change) => block_on(change_cmd::run(json, &endpoint, change)),
        Command::Commit { message, workspace } => block_on(commit_cmd::commit(
            json,
            &endpoint,
            message,
            workspace.as_deref(),
        )),
        Command::Workspace(WorkspaceCommand::Watch { workspace }) => {
            block_on(commit_cmd::watch(json, &endpoint, workspace.as_deref()))
        }
        Command::Workspace(command) => block_on(workspace_cmd::run(json, &endpoint, command)),
        Command::Log { .. } | Command::Diff(_) => history(json, &endpoint, &cli.command),
        Command::Review(review) => block_on(review_cmd::run(json, &endpoint, review)),
        Command::Receipt(receipt) => block_on(receipt_cmd::run(json, &endpoint, receipt)),
        Command::Land { change } => block_on(land_cmd::run(json, &endpoint, change)),
        Command::Overlaps(args) => block_on(overlaps_cmd::run(json, &endpoint, args)),
        Command::Status(args) => block_on(status_cmd::run(json, &endpoint, args)),
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

/// `rosen log` and `rosen diff`: read-only history queries (SPEC.md R11).
fn history(
    json: bool,
    endpoint: &Endpoint,
    command: &Command,
) -> rosen_client::Result<server::Succeeded> {
    match command {
        Command::Log {
            change,
            paths,
            limit,
        } => block_on(history_cmd::log(
            json,
            endpoint,
            change.as_deref(),
            paths,
            *limit,
        )),
        Command::Diff(args) => block_on(history_cmd::diff(
            json,
            endpoint,
            args.change.as_deref(),
            &args.paths,
        )),
        other => unreachable!("`{}` is not a history query", other.path()),
    }
}

/// Streams events for one agent, or for the whole project with `--all`.
fn events(
    json: bool,
    endpoint: &Endpoint,
    after: Option<u64>,
    agent: Option<&str>,
    all: bool,
) -> rosen_client::Result<server::Succeeded> {
    let agent = if all {
        None
    } else {
        Some(change_cmd::resolve_owner(agent)?)
    };
    block_on(server::stream_events(json, endpoint, agent, after))
}

/// The claim, share, wait or release a command asks for, if it is one.
fn path_record_call(command: &Command) -> Option<(ClaimVerb, Option<&str>, &[String])> {
    let (verb, args) = match command {
        Command::Claim(args) => (ClaimVerb::Claim, args),
        Command::Share(args) => (ClaimVerb::Share, args),
        Command::Wait(args) => (ClaimVerb::Wait, args),
        Command::Release(args) => {
            let change = args.change.as_deref();
            return Some((ClaimVerb::Release, change, &args.paths));
        }
        _ => return None,
    };
    Some((verb, args.change.as_deref(), &args.paths))
}

fn block_on<T>(
    work: impl std::future::Future<Output = rosen_client::Result<T>>,
) -> rosen_client::Result<T> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(work)
}

/// Prints a command-line error. Help and version requests stay as text; with
/// `--json` anywhere in the arguments, a usage error prints the same
/// `rosen.error/v1` object as every other failure, so an agent parsing the
/// output never meets a second error shape (SPEC.md R10).
fn report_usage_error(error: &clap::Error) -> ExitCode {
    use clap::error::ErrorKind;
    let informational = matches!(
        error.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    if informational || !std::env::args_os().any(|arg| arg == "--json") {
        error.exit();
    }
    let rendered = error.to_string();
    let message = rendered.trim_end();
    let body = serde_json::json!({
        "schema": ERROR_SCHEMA,
        "command": serde_json::Value::Null,
        "error": message.strip_prefix("error: ").unwrap_or(message),
    });
    println!("{body}");
    ExitCode::from(USAGE_EXIT)
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
