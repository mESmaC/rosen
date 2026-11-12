//! `rosen-server`: the per-machine Rosen daemon. Clients normally start it
//! through `rosen server start`; running it by hand is for debugging.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use rosen_protocol::Endpoint;
use rosen_server::ServerConfig;

/// The Rosen server daemon.
///
/// Holds one project's object store and metadata, and serves Rosen clients
/// over a local transport (a named pipe on Windows, a Unix socket elsewhere).
#[derive(Debug, Parser)]
#[command(name = "rosen-server", version)]
struct Args {
    /// Directory holding the object store and the metadata database.
    #[arg(long, value_name = "DIR", default_value = ".rosen")]
    data_dir: PathBuf,

    /// Pipe name (Windows) or socket path to listen on. Defaults to
    /// $ROSEN_ENDPOINT, then the per-user default endpoint.
    #[arg(long, value_name = "ENDPOINT")]
    endpoint: Option<String>,
}

/// The exit code when another server already owns the endpoint, distinct from a
/// failure so a launcher that lost a start race can just connect to the winner.
const EXIT_ALREADY_RUNNING: u8 = 3;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    // Clients on this machine open the object store at the data dir that
    // `status` reports (SPEC R3), so it must not depend on the daemon's cwd.
    let data_dir = std::path::absolute(&args.data_dir).unwrap_or(args.data_dir);
    let config = ServerConfig {
        data_dir,
        endpoint: args.endpoint.map_or_else(Endpoint::resolve, Endpoint::new),
    };
    // Ctrl-C stops the daemon as a `shutdown` request would.
    let result = tokio::select! {
        result = rosen_server::serve(&config) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "rosen-server: a server is already running on {}",
                config.endpoint
            );
            ExitCode::from(EXIT_ALREADY_RUNNING)
        }
        Err(err) => {
            eprintln!("rosen-server: {err}");
            ExitCode::FAILURE
        }
    }
}
