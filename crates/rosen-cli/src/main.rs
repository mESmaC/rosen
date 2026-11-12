//! `rosen`: the command-line client, a thin layer over `rosen-client` (SPEC R10).

use clap::{Args, Parser, Subcommand};
use rosen_client::protocol::{Event, ServerStatus};
use rosen_client::{Client, ClientError, Endpoint};
use serde_json::json;
use std::process::ExitCode;

/// Rosen is a version control system for many coding agents working on one
/// project at once. A per-machine server holds the project and does merges,
/// rebases and landing; this client talks to it. Most commands start the server
/// automatically when it is not running.
#[derive(Parser)]
#[command(name = "rosen", version)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct GlobalArgs {
    /// Print machine-readable JSON. Every object carries a "schema" field such as
    /// "rosen.server.status/1"; the number after the slash changes only when a
    /// field is removed or changes meaning.
    #[arg(long, global = true)]
    json: bool,

    /// Server pipe name (Windows) or socket path. Defaults to $ROSEN_ENDPOINT,
    /// then the per-user default, so normally you never set it.
    #[arg(long, global = true, value_name = "ENDPOINT")]
    endpoint: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the per-machine Rosen server.
    #[command(subcommand)]
    Server(ServerCommand),

    /// Stream server events (snapshot, landed, rebased, conflict, updated,
    /// review, claim, released, green, reverted, mirror) until interrupted.
    ///
    /// Each event has a sequence number. After a disconnect, pass the last one
    /// you saw to --after to resume without missing or repeating events.
    Events {
        /// Deliver only events with a sequence number greater than this.
        #[arg(long, value_name = "SEQ")]
        after: Option<u64>,
    },
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Start the server in the background if it is not running, then print its
    /// status. Safe to run when a server is already up.
    Start,
    /// Print the running server's status. Exits with status 1, without starting
    /// a server, when none is running.
    Status,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let endpoint = cli
        .global
        .endpoint
        .clone()
        .map_or_else(Endpoint::resolve, Endpoint::new);
    let result = match cli.command {
        Command::Server(ServerCommand::Start) => server_start(&cli.global, &endpoint).await,
        Command::Server(ServerCommand::Status) => server_status(&cli.global, &endpoint).await,
        Command::Events { after } => stream_events(&cli.global, &endpoint, after).await,
    };
    match result {
        Ok(code) => code,
        Err(err) => report_error(&cli.global, &err),
    }
}

async fn server_start(global: &GlobalArgs, endpoint: &Endpoint) -> Result<ExitCode, ClientError> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    print_status(global, &client.status().await?);
    Ok(ExitCode::SUCCESS)
}

async fn server_status(global: &GlobalArgs, endpoint: &Endpoint) -> Result<ExitCode, ClientError> {
    match Client::connect(endpoint).await {
        Ok(client) => {
            print_status(global, &client.status().await?);
            Ok(ExitCode::SUCCESS)
        }
        Err(ClientError::NotRunning(_)) => {
            if global.json {
                let body = json!({"schema": "rosen.server.status/1", "running": false, "endpoint": endpoint.as_str()});
                println!("{body}");
            } else {
                println!("not running ({endpoint})");
            }
            Ok(ExitCode::FAILURE)
        }
        Err(err) => Err(err),
    }
}

fn print_status(global: &GlobalArgs, status: &ServerStatus) {
    if global.json {
        let body = json!({
            "schema": "rosen.server.status/1",
            "running": true,
            "server": status.server,
            "protocol_version": status.protocol_version,
            "pid": status.pid,
            "endpoint": status.endpoint,
            "uptime_ms": status.uptime_ms,
            "open_connections": status.open_connections,
            "last_event": status.last_event,
        });
        println!("{body}");
    } else {
        println!("running: {} (pid {})", status.server, status.pid);
        println!("endpoint: {}", status.endpoint);
        println!("protocol: v{}", status.protocol_version);
        println!("uptime: {:.1}s", status.uptime_ms as f64 / 1000.0);
        println!("connections: {}", status.open_connections);
        println!("last event: {}", status.last_event);
    }
}

async fn stream_events(
    global: &GlobalArgs,
    endpoint: &Endpoint,
    after: Option<u64>,
) -> Result<ExitCode, ClientError> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    let mut events = client.subscribe(after).await?;
    while let Some(event) = events.recv().await {
        print_event(global, &event);
    }
    // The stream only ends when the server goes away.
    Err(ClientError::Disconnected)
}

fn print_event(global: &GlobalArgs, event: &Event) {
    if global.json {
        let body = json!({
            "schema": "rosen.event/1",
            "seq": event.seq,
            "kind": event.kind,
            "change": event.change,
            "detail": event.detail,
        });
        println!("{body}");
    } else {
        let change = event.change.as_deref().unwrap_or("-");
        println!("{} {:?} {} {}", event.seq, event.kind, change, event.detail);
    }
}

fn report_error(global: &GlobalArgs, err: &ClientError) -> ExitCode {
    if global.json {
        let body = json!({"schema": "rosen.error/1", "error": error_code(err), "message": err.to_string()});
        println!("{body}");
    } else {
        eprintln!("rosen: {err}");
    }
    ExitCode::from(2)
}

/// A stable name for each failure, for scripts that branch on `--json` errors.
fn error_code(err: &ClientError) -> &'static str {
    match err {
        ClientError::NotRunning(_) => "not_running",
        ClientError::Start(_) => "start_failed",
        ClientError::Rejected { .. } => "rejected",
        ClientError::Server(_) => "server_error",
        ClientError::Disconnected => "disconnected",
        ClientError::Unexpected(_) => "protocol_error",
        ClientError::Io(_) => "io_error",
    }
}
