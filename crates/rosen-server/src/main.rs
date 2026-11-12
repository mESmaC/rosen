//! `rosen-server`: the per-machine Rosen daemon. Clients normally start it
//! through `rosen server start`; running it by hand is for debugging.

use std::path::PathBuf;

use clap::Parser;
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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    rosen_server::serve(&ServerConfig {
        data_dir: args.data_dir,
    })
}
