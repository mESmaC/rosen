//! `rosen-server`: the per-machine Rosen daemon. Clients normally start it
//! through `rosen server start`; running it by hand is for debugging.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use daemon_log::DaemonLog;
use rosen_protocol::Endpoint;
use rosen_server::network::identity::parse_fingerprint;
use rosen_server::network::{NetworkOptions, UpstreamConfig, LISTEN_ENV};
use rosen_server::ServerConfig;

mod daemon_log;

/// The Rosen server daemon.
///
/// Holds one project's object store and metadata, and serves Rosen clients
/// over a local transport (a named pipe on Windows, a Unix socket elsewhere),
/// and daemons on other machines over TLS with `--listen`. With an upstream
/// server (`--upstream`, or `upstream.json` in the data directory) it runs
/// instead as this machine's object cache in front of that server.
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

    /// Also listen for other machines' daemons on this TCP address, with
    /// TLS, for example 0.0.0.0:7420. Defaults to $ROSEN_LISTEN.
    #[arg(long, value_name = "ADDR", conflicts_with = "upstream")]
    listen: Option<String>,

    /// Run as this machine's cache in front of the server listening here.
    #[arg(long, value_name = "HOST:PORT", requires_all = ["fingerprint", "credential_file"])]
    upstream: Option<String>,

    /// The upstream server's certificate fingerprint (SHA-256, hex).
    #[arg(long, value_name = "HEX", requires = "upstream")]
    fingerprint: Option<String>,

    /// A file holding the machine credential to present to the server.
    #[arg(long, value_name = "FILE", requires = "upstream")]
    credential_file: Option<PathBuf>,

    /// Keep the warm pool in a directory under DIR. A take is served only
    /// when DIR is on the same volume as the workspace, so put it on the
    /// volume agents' workspaces live on, such as a Dev Drive. Defaults to
    /// $ROSEN_WARM_POOL_DIR, then the data directory.
    #[arg(long, value_name = "DIR")]
    warm_pool_dir: Option<PathBuf>,

    /// Append this daemon's diagnostics (its standard error) to FILE,
    /// relative to the data directory. A client that starts the daemon
    /// passes `--log daemon.log`: that daemon has no console, so without a
    /// log a pool worker's wait report or a failed prepare's output is lost.
    #[arg(long, value_name = "FILE")]
    log: Option<PathBuf>,
}

/// Inherited by a daemon a client starts with no flags, as `ROSEN_LISTEN` is.
const WARM_POOL_DIR_ENV: &str = "ROSEN_WARM_POOL_DIR";

impl Args {
    /// Flags win; otherwise a cache reads `upstream.json` and a server reads
    /// `$ROSEN_LISTEN`, so a daemon a client started with no flags still runs
    /// in the right role.
    fn network(&self, data_dir: &Path) -> Result<NetworkOptions, String> {
        if let Some(address) = &self.upstream {
            let fingerprint = self.fingerprint.as_deref().unwrap_or_default();
            let credential_file = self.credential_file.as_deref().unwrap_or(Path::new(""));
            let credential = std::fs::read_to_string(credential_file)
                .map_err(|err| format!("{}: {err}", credential_file.display()))?;
            let upstream = UpstreamConfig {
                address: address.clone(),
                fingerprint: parse_fingerprint(fingerprint)?,
                credential: credential.trim().to_owned(),
            };
            return Ok(NetworkOptions {
                listen: None,
                upstream: Some(upstream),
            });
        }
        if let Some(upstream) = UpstreamConfig::load(data_dir).map_err(|err| err.to_string())? {
            return Ok(NetworkOptions {
                listen: None,
                upstream: Some(upstream),
            });
        }
        let listen = self.listen.clone().or_else(|| {
            std::env::var(LISTEN_ENV)
                .ok()
                .filter(|address| !address.is_empty())
        });
        Ok(NetworkOptions {
            listen,
            upstream: None,
        })
    }
}

/// The exit code when another server already owns the endpoint, distinct from a
/// failure so a launcher that lost a start race can just connect to the winner.
const EXIT_ALREADY_RUNNING: u8 = 3;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    // Clients on this machine open the object store at the data dir that
    // `status` reports (SPEC R3), so it must not depend on the daemon's cwd.
    let data_dir = std::path::absolute(&args.data_dir).unwrap_or_else(|_| args.data_dir.clone());
    if let Some(log) = &args.log {
        let path = data_dir.join(log);
        match DaemonLog::open(&path) {
            Ok(log) => {
                eprintln!(
                    "rosen-server {}: started, process {}",
                    env!("CARGO_PKG_VERSION"),
                    std::process::id()
                );
                log.keep_rotating();
            }
            Err(err) => eprintln!("rosen-server: cannot log to {}: {err}", path.display()),
        }
    }
    let network = match args.network(&data_dir) {
        Ok(network) => network,
        Err(err) => {
            eprintln!("rosen-server: {err}");
            return ExitCode::FAILURE;
        }
    };
    let warm_pool_dir = args
        .warm_pool_dir
        .or_else(|| std::env::var_os(WARM_POOL_DIR_ENV).map(PathBuf::from))
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| std::path::absolute(&dir).unwrap_or(dir));
    let config = ServerConfig {
        data_dir,
        endpoint: args.endpoint.map_or_else(Endpoint::resolve, Endpoint::new),
        warm_pool_dir,
    };
    // Ctrl-C stops the daemon as a `shutdown` request would.
    let result = tokio::select! {
        result = rosen_server::serve_with(&config, network) => result,
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
