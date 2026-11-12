//! The Rosen daemon (SPEC.md D8, §3).
//!
//! One server runs per machine. It holds the authoritative object store and
//! SQLite metadata store, and does every merge, rebase, land and history
//! query on behalf of thin clients.

use std::path::PathBuf;

/// Where a server keeps its state.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Directory holding the object store and the metadata database.
    pub data_dir: PathBuf,
}

/// Runs the daemon until it is shut down.
pub fn serve(config: &ServerConfig) -> anyhow::Result<()> {
    anyhow::bail!(
        "the server for {} cannot listen yet: the CLI-to-server protocol is not implemented",
        config.data_dir.display()
    )
}
