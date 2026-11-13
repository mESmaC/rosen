//! The processes of a stress run on one computer standing in for several
//! machines: the server as machine 0, and for every other machine a cache
//! daemon enrolled with the run's one join key (R14), reaching the server
//! through its own partition proxy. Each machine's agents run as one
//! `rosen-stress agents` process, which a fault can kill and restart.

use crate::machine::connect_when_up;
use anyhow::{bail, Context, Result};
use rosen_client::protocol::{CreateJoinKey, MachineRole};
use rosen_client::Endpoint;
use rosen_faults::execute::Faults;
use rosen_faults::plan::{Cut, Machine, Target};
use rosen_faults::process::{Recipe, Slot};
use rosen_faults::proxy::PartitionProxy;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Where each machine keeps its data and what its endpoint is called.
pub struct Layout {
    pub root: PathBuf,
    pub machines: usize,
    /// Unique per run, so its pipes never meet another run's.
    tag: String,
}

impl Layout {
    pub fn new(root: &Path, machines: usize) -> Self {
        Self {
            root: root.to_path_buf(),
            machines,
            tag: format!("rosen-stress-{}", std::process::id()),
        }
    }

    pub fn machine_name(machine: Machine) -> String {
        format!("m{machine}")
    }

    pub fn machine_dir(&self, machine: Machine) -> PathBuf {
        self.root.join(Self::machine_name(machine))
    }

    pub fn endpoint(&self, machine: Machine) -> Endpoint {
        let name = format!("{}-{}", self.tag, Self::machine_name(machine));
        if cfg!(windows) {
            Endpoint::new(name)
        } else {
            Endpoint::new(self.root.join(format!("{name}.sock")).to_string_lossy())
        }
    }

    pub fn report(&self, machine: Machine) -> PathBuf {
        self.root.join(format!("report-{}.json", Self::machine_name(machine)))
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
}

pub struct Cluster {
    server: Slot,
    proxies: Vec<PartitionProxy>,
    daemons: Vec<Slot>,
    agents: Vec<Slot>,
    logs: PathBuf,
}

impl Cluster {
    /// Starts the server and waits until it answers.
    pub async fn start_server(layout: &Layout) -> Result<(Self, SocketAddr)> {
        std::fs::create_dir_all(layout.logs())?;
        let listen: SocketAddr = format!("127.0.0.1:{}", free_port()?).parse()?;
        let mut server = Slot::new("server", server_recipe(layout, listen))
            .log_to(layout.logs().join("server.log"));
        server.start()?;
        connect_when_up(&layout.endpoint(0), Duration::from_secs(30)).await?;
        let cluster = Self {
            server,
            proxies: Vec::new(),
            daemons: Vec::new(),
            agents: Vec::new(),
            logs: layout.logs(),
        };
        Ok((cluster, listen))
    }

    /// Enrolls every other machine with one join key, as a provisioning
    /// script would, then starts its daemon behind its own proxy.
    pub async fn start_daemons(&mut self, layout: &Layout, listen: SocketAddr) -> Result<()> {
        if layout.machines < 2 {
            return Ok(());
        }
        let server = connect_when_up(&layout.endpoint(0), Duration::from_secs(30)).await?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let key = server
            .create_join_key(CreateJoinKey {
                address: listen.to_string(),
                expires_at: now + 3600,
                use_limit: (layout.machines - 1) as u32,
                role: MachineRole::Agent,
            })
            .await
            .context("creating the run's join key")?;
        for machine in 1..layout.machines {
            let dir = layout.machine_dir(machine);
            let name = format!("{}-{}", layout.tag, Layout::machine_name(machine));
            rosen_client::enrollment::join(&dir, &key, &name)
                .await
                .with_context(|| format!("enrolling machine {machine}"))?;
            let proxy = PartitionProxy::start("127.0.0.1:0".parse()?, listen).await?;
            route_through(&dir, proxy.address())?;
            let log = self.logs.join(format!("daemon-{machine}.log"));
            let mut daemon = Slot::new(format!("daemon-{machine}"), daemon_recipe(layout, machine)).log_to(log);
            daemon.start()?;
            connect_when_up(&layout.endpoint(machine), Duration::from_secs(30)).await?;
            self.proxies.push(proxy);
            self.daemons.push(daemon);
        }
        Ok(())
    }

    /// Starts one `agents` process per machine, each with its recipe.
    pub fn start_agents(&mut self, recipes: Vec<Recipe>) -> Result<()> {
        for (machine, recipe) in recipes.into_iter().enumerate() {
            let name = format!("agents-{machine}");
            let log = self.logs.join(format!("{name}.log"));
            self.agents.push(Slot::new(name, recipe).log_to(log));
            self.start_agent(machine)?;
        }
        Ok(())
    }

    /// Starts machine `machine`'s agents, copying their output to its log.
    fn start_agent(&mut self, machine: Machine) -> Result<()> {
        let slot = &mut self.agents[machine];
        let log = self.logs.join(format!("{}.out", slot.name));
        if let Some(mut stdout) = slot.start()? {
            // Drained, so the process never blocks or fails writing its
            // summary; each start's output is appended to the log.
            tokio::spawn(async move {
                let mut output = Vec::new();
                let _ = stdout.read_to_end(&mut output).await;
                let file = std::fs::OpenOptions::new().create(true).append(true).open(&log);
                if let Ok(mut file) = file {
                    let _ = std::io::Write::write_all(&mut file, &output);
                }
            });
        }
        Ok(())
    }

    /// Waits until every `agents` process has exited, and fails when one
    /// failed or is still running at the deadline: a hang shows up so.
    pub async fn wait_for_agents(&mut self, deadline: Duration) -> Result<()> {
        let give_up = tokio::time::Instant::now() + deadline;
        loop {
            let running: Vec<String> = self
                .agents
                .iter_mut()
                .filter_map(|slot| slot.running().then(|| slot.name.clone()))
                .collect();
            if running.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= give_up {
                bail!("still running after {deadline:?}: {}", running.join(", "));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        for slot in &mut self.agents {
            match slot.exited() {
                Some(status) if status.success() => {}
                status => bail!("{} ended with {status:?}", slot.name),
            }
        }
        Ok(())
    }

    /// Exits nobody planned: the server and daemons never exit on their
    /// own, and an `agents` process only exits unsuccessfully when it fails.
    pub fn crashed(&mut self) -> Vec<String> {
        let mut crashed = Vec::new();
        for slot in std::iter::once(&mut self.server).chain(self.daemons.iter_mut()) {
            let last = slot.exited();
            for status in slot.own_exits.iter().chain(&last) {
                crashed.push(format!("{} exited with {status}", slot.name));
            }
        }
        for slot in &self.agents {
            for status in slot.own_exits.iter().filter(|status| !status.success()) {
                crashed.push(format!("{} exited with {status}", slot.name));
            }
        }
        crashed
    }

    /// Kills and restarts each process counted, for the run's summary.
    pub fn counts(&self) -> Vec<(String, u32, u32)> {
        std::iter::once(&self.server)
            .chain(&self.daemons)
            .chain(&self.agents)
            .map(|slot| (slot.name.clone(), slot.kills, slot.starts))
            .collect()
    }

    /// Stops every process for good and removes the machine credentials
    /// enrollment stored, so runs leave nothing in the credential vault.
    pub async fn stop(&mut self, layout: &Layout) {
        for slot in self.agents.iter_mut().chain(&mut self.daemons) {
            slot.stop().await;
        }
        if let Ok(client) = rosen_client::Client::connect(&layout.endpoint(0)).await {
            let _ = client.shutdown().await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.server.stop().await;
        for machine in 1..layout.machines {
            if let Err(err) = forget(&layout.machine_dir(machine)) {
                eprintln!("rosen-stress: machine {machine}'s credential: {err:#}");
            }
        }
    }
}

impl Faults for Cluster {
    async fn kill(&mut self, target: Target) -> Result<()> {
        match target {
            Target::Server => self.server.kill().await,
            Target::Daemon { machine } => self.daemons[machine - 1].kill().await,
            Target::Client { machine, .. } => self.agents[machine].kill().await,
        }
    }

    async fn restart(&mut self, target: Target) -> Result<()> {
        match target {
            Target::Server => self.server.start().map(drop),
            Target::Daemon { machine } => self.daemons[machine - 1].start().map(drop),
            Target::Client { machine, .. } => self.start_agent(machine),
        }
    }

    fn cut(&mut self, machine: Machine, cut: Cut) {
        self.proxies[machine - 1].cut(cut);
    }

    fn heal(&mut self, machine: Machine) {
        self.proxies[machine - 1].heal();
    }
}

/// Points an enrolled machine's daemon at its partition proxy instead of
/// the server. The proxy relays bytes untouched, so TLS and certificate
/// pinning still run end to end; cutting the proxy cuts exactly this
/// machine off, as losing its route to the server would.
fn route_through(dir: &Path, proxy: SocketAddr) -> Result<()> {
    let path = dir.join("upstream.json");
    let mut json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    json["address"] = serde_json::Value::String(proxy.to_string());
    std::fs::write(&path, serde_json::to_vec(&json)?)?;
    Ok(())
}

fn server_recipe(layout: &Layout, listen: SocketAddr) -> Recipe {
    let bin = rosen_client::daemon::server_binary();
    let dir = layout.machine_dir(0);
    let endpoint = layout.endpoint(0);
    Box::new(move || {
        let mut command = Command::new(&bin);
        command
            .arg("--data-dir")
            .arg(&dir)
            .args(["--endpoint", endpoint.as_str(), "--listen", &listen.to_string()]);
        command
    })
}

/// The daemon finds its server and credential in `upstream.json`, as a
/// daemon a client starts with no flags does.
fn daemon_recipe(layout: &Layout, machine: Machine) -> Recipe {
    let bin = rosen_client::daemon::server_binary();
    let dir = layout.machine_dir(machine);
    let endpoint = layout.endpoint(machine);
    Box::new(move || {
        let mut command = Command::new(&bin);
        command
            .arg("--data-dir")
            .arg(&dir)
            .args(["--endpoint", endpoint.as_str()]);
        command
    })
}

/// A free port for the server's listener, kept across its restarts so the
/// proxies keep reaching it. It is drawn below Windows' ephemeral range
/// (49152 and up): an outgoing connection could take an ephemeral port
/// while the server is down, and the restarted server could not listen.
fn free_port() -> Result<u16> {
    let start = 20_000 + (std::process::id() % 20_000) as u16;
    (0..20_000u16)
        .map(|offset| 20_000 + (start - 20_000 + offset) % 20_000)
        .find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok())
        .context("no free port for the server between 20000 and 40000")
}

/// Removes the machine credential enrollment stored for `dir`.
fn forget(dir: &Path) -> Result<()> {
    let path = dir.join("upstream.json");
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let reference = json["credential"].as_str().unwrap_or_default();
    if let Some(target) = reference.strip_prefix("credman:") {
        delete_credential(target)?;
    }
    Ok(())
}

#[cfg(windows)]
fn delete_credential(target: &str) -> Result<()> {
    Ok(rosen_platform::credentials::delete(target)?)
}

#[cfg(not(windows))]
fn delete_credential(_: &str) -> Result<()> {
    Ok(())
}
