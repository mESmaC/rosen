//! `rosen-server`: the per-machine Rosen daemon. Clients normally start it
//! through `rosen server start`; running it by hand is for debugging.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use rosen_protocol::Endpoint;
use rosen_server::network::identity::parse_fingerprint;
use rosen_server::network::{NetworkOptions, UpstreamConfig, LISTEN_ENV};
use rosen_server::ServerConfig;

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
}

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
    let network = match args.network(&data_dir) {
        Ok(network) => network,
        Err(err) => {
            eprintln!("rosen-server: {err}");
            return ExitCode::FAILURE;
        }
    };
    let config = ServerConfig {
        data_dir,
        endpoint: args.endpoint.map_or_else(Endpoint::resolve, Endpoint::new),
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
