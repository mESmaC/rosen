//! `rosen-server`: runs the Rosen daemon in the foreground. `rosen server start`
//! (or `rosen-client`) normally launches it in the background.

use clap::Parser;
use rosen_protocol::Endpoint;
use std::process::ExitCode;

/// The Rosen daemon. One runs per machine and user; clients start it on demand.
#[derive(Parser)]
#[command(name = "rosen-server", version)]
struct Args {
    /// Pipe name (Windows) or socket path to listen on. Defaults to
    /// $ROSEN_ENDPOINT, then the per-user default endpoint.
    #[arg(long)]
    endpoint: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let endpoint = args.endpoint.map_or_else(Endpoint::resolve, Endpoint::new);
    match serve_until_stopped(endpoint.clone()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("rosen-server: a server is already running on {endpoint}");
            // Distinct from a failure, so a launcher that raced another can tell.
            ExitCode::from(3)
        }
        Err(err) => {
            eprintln!("rosen-server: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the daemon, stopping on a `shutdown` request or Ctrl-C.
async fn serve_until_stopped(endpoint: Endpoint) -> std::io::Result<()> {
    tokio::select! {
        result = rosen_server::run(endpoint) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
