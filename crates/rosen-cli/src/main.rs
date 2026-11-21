//! `rosen`: the Rosen command-line tool, a thin layer over `rosen-client`.

mod change_cmd;
mod claim_cmd;
mod collapse_cmd;
mod collapse_config;
mod collapse_git;
mod commands;
mod commit_cmd;
mod config_cmd;
mod history_cmd;
mod import;
mod land_cmd;
mod machine_cmd;
mod machine_list;
mod managed;
mod mcp;
mod mcp_tools;
mod mirror_cmd;
mod output;
mod overlaps_cmd;
mod rebase_cmd;
mod receipt_cmd;
mod resolve_cmd;
mod review_cmd;
mod review_status;
mod run_cmd;
mod server;
mod status_cmd;
mod status_next;
mod workspace_cmd;

use std::process::ExitCode;

use claim_cmd::ClaimVerb;
use clap::FromArgMatches;
use commands::{Cli, Command, ServerCommand, WorkspaceCommand};
use rosen_client::Endpoint;

/// Exit status for an invalid command line, as clap uses.
const USAGE_EXIT: u8 = 2;

fn main() -> ExitCode {
    let managed = managed::is_managed();
    let matches = match commands::command(managed).try_get_matches() {
        Ok(matches) => matches,
        Err(error) => return report_usage_error(&error),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => return report_usage_error(&error),
    };
    if cli.json_schema {
        println!("{}", output::CATALOG);
        return ExitCode::SUCCESS;
    }
    let Some(command) = &cli.command else {
        return report_usage_error(&commands::command(managed).error(
            clap::error::ErrorKind::MissingSubcommand,
            "a command is required",
        ));
    };
    if let Command::Run(args) = command {
        return run_in_turn(cli.json, args);
    }
    let operation = command.path();
    match run(&cli, command) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            report_error(cli.json, operation, &error);
            ExitCode::FAILURE
        }
    }
}

/// `rosen run`: exits with the command's own status, so a script sees the
/// build's failure and not just "failed".
fn run_in_turn(json: bool, args: &commands::RunArgs) -> ExitCode {
    if let Some(settings) = &args.slots {
        return match run_cmd::slots(settings) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                report_error(json, "run", &rosen_client::Error::Io(error));
                ExitCode::FAILURE
            }
        };
    }
    match run_cmd::run(json, args) {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => match status.code().map(u8::try_from) {
            Some(Ok(code)) => ExitCode::from(code),
            // Ended by a signal, or a status a process can't pass on.
            _ => ExitCode::FAILURE,
        },
        Err(error) => {
            report_error(json, "run", &rosen_client::Error::Io(error));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli, command: &Command) -> rosen_client::Result<server::Succeeded> {
    let endpoint = cli
        .endpoint
        .clone()
        .map_or_else(Endpoint::resolve, Endpoint::new);
    let json = cli.json;
    if let Some((verb, change, paths)) = path_record_call(command) {
        return block_on(claim_cmd::run(json, &endpoint, verb, change, paths));
    }
    match command {
        Command::Server(ServerCommand::Start) => block_on(server::start(json, &endpoint)),
        Command::Server(ServerCommand::Status) => block_on(server::status(json, &endpoint)),
        Command::Server(ServerCommand::Stop) => block_on(server::stop(json, &endpoint)),
        Command::Server(ServerCommand::Relay { allow }) => {
            block_on(server::relay(json, &endpoint, allow))
        }
        Command::Events {
            after,
            agent,
            all,
            once,
            limit,
        } => events(
            json,
            &endpoint,
            *after,
            agent.as_deref(),
            *all,
            *once,
            *limit,
        ),
        Command::Config(config) => block_on(config_cmd::run(json, &endpoint, config)),
        Command::Mirror(commands::MirrorCommand::Adopt { commit }) => {
            block_on(mirror_cmd::adopt(json, &endpoint, *commit))
        }
        Command::Mirror(commands::MirrorCommand::CatchUp) => {
            block_on(mirror_cmd::catch_up(json, &endpoint))
        }
        Command::Mirror(commands::MirrorCommand::DropPaths { paths }) => {
            block_on(mirror_cmd::drop_paths(json, &endpoint, paths))
        }
        Command::Collapse(commands::CollapseCommand::Check) => {
            block_on(collapse_cmd::check(json, &endpoint))
        }
        Command::Collapse(commands::CollapseCommand::Link(args)) => {
            block_on(collapse_cmd::link(json, &endpoint, args))
        }
        Command::Collapse(commands::CollapseCommand::Unlink(args)) => {
            block_on(collapse_cmd::unlink(json, &endpoint, args))
        }
        Command::Machine(command) => block_on(machine_cmd::run(json, &endpoint, command)),
        Command::Mcp(args) => {
            mcp::serve(args, cli.endpoint.as_deref())?;
            Ok(true)
        }
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
        Command::Log { .. } | Command::Diff(_) => history(json, &endpoint, command),
        Command::Review(review) => block_on(review_cmd::run(json, &endpoint, review)),
        Command::Receipt(receipt) => block_on(receipt_cmd::run(json, &endpoint, receipt)),
        Command::Land {
            change,
            check: true,
        } => block_on(land_cmd::check(json, &endpoint, change)),
        Command::Land { change, .. } => block_on(land_cmd::run(json, &endpoint, change)),
        Command::Rebase { change } => block_on(rebase_cmd::run(json, &endpoint, change.as_deref())),
        Command::Resolve { paths, change } => {
            block_on(resolve_cmd::run(json, &endpoint, change.as_deref(), paths))
        }
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
/// With `--once`, drains pending events up to now and exits.
fn events(
    json: bool,
    endpoint: &Endpoint,
    after: Option<u64>,
    agent: Option<&str>,
    all: bool,
    once: bool,
    limit: Option<usize>,
) -> rosen_client::Result<server::Succeeded> {
    let agent = if all {
        None
    } else {
        Some(change_cmd::resolve_owner(agent)?)
    };
    if once {
        block_on(server::poll_events(json, endpoint, agent, after, limit))
    } else {
        block_on(server::stream_events(json, endpoint, agent, after))
    }
}

/// The claim, share, wait or release a command asks for, if it is one.
fn path_record_call(command: &Command) -> Option<(ClaimVerb, Option<&str>, &[String])> {
    let (verb, args) = match command {
        Command::Claim { paths, exclusive } => (
            ClaimVerb::Claim {
                exclusive: *exclusive,
            },
            paths,
        ),
        Command::Share(args) => (ClaimVerb::Share, args),
        Command::Wait(args) => (ClaimVerb::Wait { park: args.park }, &args.claim),
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
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    );
    if informational || !std::env::args_os().any(|arg| arg == "--json") {
        error.exit();
    }
    let rendered = error.to_string();
    let message = rendered.trim_end();
    output::print_error(None, message.strip_prefix("error: ").unwrap_or(message));
    ExitCode::from(USAGE_EXIT)
}

fn report_error(json: bool, operation: &str, error: &rosen_client::Error) {
    if json {
        output::print_error(Some(operation), &error.to_string());
    } else {
        eprintln!("rosen: {error}");
    }
}
