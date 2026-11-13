//! Lays out the drill's machines on one disk and creates the project.

use anyhow::{anyhow, Context};
use rosen_core::{Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_server::landing::Ledger;
use rosen_server::network::identity::{self, Identity};
use rosen_server::ServerConfig;
use rosen_store::{LooseObjectStore, ObjectStore};
use std::path::{Path, PathBuf};

/// Where each simulated machine keeps its data, and the names of its
/// daemon's local endpoint.
pub struct Layout {
    pub root: PathBuf,
    /// Unique per run, so drills never meet another run's pipes.
    pub tag: String,
}

impl Layout {
    pub fn machine_dir(&self, machine: usize) -> PathBuf {
        self.root.join(format!("machine-{machine}"))
    }

    /// Machine 0's daemon is the server.
    pub fn server_dir(&self) -> PathBuf {
        self.machine_dir(0)
    }

    pub fn endpoint(&self, machine: usize) -> String {
        format!("rosen-faults-{}-m{machine}", self.tag)
    }

    pub fn server_config(&self) -> ServerConfig {
        ServerConfig {
            data_dir: self.server_dir(),
            endpoint: rosen_protocol::Endpoint::new(self.endpoint(0)),
        }
    }
}

/// The directory and build unit of one agent.
pub fn unit(machine: usize, index: usize) -> String {
    format!("m{machine}a{index}")
}

/// Creates the base: one commit whose `rosen.toml` makes each agent's
/// directory its own build unit. Agents' changes are then independent, so a
/// receipt survives other agents' lands (R8) and the drill's lands race on
/// the compare-and-swap rather than queueing for re-checks.
pub fn create_project(layout: &Layout, units: &[String]) -> anyhow::Result<ObjectId> {
    let config = layout.server_config();
    std::fs::create_dir_all(&config.data_dir)?;
    let store = LooseObjectStore::open(config.objects_dir())?;
    let toml: String = units
        .iter()
        .map(|unit| format!("[build_units.{unit}]\npaths = ['{unit}/**']\n"))
        .collect();
    let blob = store.put_blob(toml.as_bytes())?;
    let entry = TreeEntry::new(FileMode::Regular, rosen_core::CONFIG_PATH, blob)
        .map_err(|err| anyhow!("{err}"))?;
    let tree = Tree::new(vec![entry]).map_err(|err| anyhow!("{err}"))?;
    let who = Signature::new("rosen-faults", "rosen-faults@localhost", 0);
    let root = store.put_commit(&Commit {
        tree: store.put_tree(&tree)?,
        parents: vec![],
        author: who.clone(),
        committer: who,
        extra_headers: vec![],
        message: b"stress base\n".to_vec(),
    })?;
    let ledger = Ledger::open(&config.metadata_path()).map_err(|err| anyhow!("{err}"))?;
    ledger
        .init_base("main", &root)
        .map_err(|err| anyhow!("{err}"))?;
    Ok(root)
}

/// The server's fingerprint and machine credential, created now so cache
/// daemons can be configured before the server first starts.
pub fn server_identity(server_dir: &Path) -> anyhow::Result<(String, String)> {
    let identity = Identity::load_or_create(server_dir).context("server identity")?;
    Ok((
        identity::hex(&identity.fingerprint()),
        identity.credential().to_owned(),
    ))
}

/// A free local port for the server's listener. It must stay the same
/// across server restarts, so the proxies keep pointing at it. It is drawn
/// below Windows' ephemeral range (49152 and up): an ephemeral port could be
/// taken by an outgoing connection while the server is down, and then the
/// restarted server could not listen.
pub fn free_port() -> anyhow::Result<u16> {
    let mut rng = crate::rng::Rng::new(crate::rng::Rng::fresh_seed());
    for _ in 0..100 {
        let port = 20_000 + rng.below(20_000) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!("no free port for the server between 20000 and 40000")
}

/// The `rosen-server` binary: next to this one unless named.
pub fn server_binary(named: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = named {
        return Ok(path);
    }
    let me = std::env::current_exe()?;
    let path = me.with_file_name(format!("rosen-server{}", std::env::consts::EXE_SUFFIX));
    if !path.exists() {
        anyhow::bail!(
            "{} not found; build it with `cargo build -p rosen-server` or pass --server-bin",
            path.display()
        );
    }
    Ok(path)
}
