//! The drill's processes: one server, one cache daemon per other machine
//! behind its partition proxy, and agent processes on every machine.

use super::agent::Ack;
use super::enroll;
use super::setup::{self, Layout};
use crate::execute::Faults;
use crate::plan::{Cut, Machine, Target};
use crate::process::Slot;
use crate::proxy::PartitionProxy;
use anyhow::{bail, Context};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::process::{ChildStdout, Command};

/// Every ack any agent printed, with the agent's name.
pub type Acks = Arc<Mutex<Vec<(String, Ack)>>>;

pub struct Spec {
    pub layout: Layout,
    pub server_bin: PathBuf,
    pub machines: usize,
    pub agents_per_machine: usize,
    pub lands: usize,
    /// The agent that lands the injected failing commit, and as which land.
    pub culprit: Option<(Machine, usize, usize)>,
}

pub struct Harness {
    server: Slot,
    listen: SocketAddr,
    /// Index `m - 1` holds machine `m`'s proxy and daemon.
    proxies: Vec<PartitionProxy>,
    daemons: Vec<Slot>,
    agents: Vec<Vec<Slot>>,
    pub acks: Acks,
}

impl Harness {
    pub async fn start(spec: &Spec) -> anyhow::Result<Self> {
        let listen: SocketAddr = format!("127.0.0.1:{}", setup::free_port()?).parse()?;
        let logs = spec.layout.root.join("logs");
        std::fs::create_dir_all(&logs)?;
        let mut server = Slot::new("server", server_recipe(spec, listen)).log_to(logs.join("server.log"));
        server.start()?;
        wait_ready(&spec.layout.endpoint(0)).await?;
        let mut proxies = Vec::new();
        let mut daemons = Vec::new();
        for machine in 1..spec.machines {
            let proxy = PartitionProxy::start("127.0.0.1:0".parse()?, listen).await?;
            let dir = spec.layout.machine_dir(machine);
            let server = rosen_protocol::Endpoint::new(spec.layout.endpoint(0));
            let name = format!("daemon-{machine}");
            enroll::enroll(&server, proxy.address(), &dir, &format!("{}-m{machine}", spec.layout.tag)).await?;
            let log = logs.join(format!("{name}.log"));
            let mut daemon = Slot::new(name, daemon_recipe(spec, machine)).log_to(log);
            daemon.start()?;
            wait_ready(&spec.layout.endpoint(machine)).await?;
            proxies.push(proxy);
            daemons.push(daemon);
        }
        let mut harness = Self {
            server,
            listen,
            proxies,
            daemons,
            agents: agent_slots(spec)?,
            acks: Arc::default(),
        };
        for machine in 0..spec.machines {
            for index in 0..spec.agents_per_machine {
                harness.start_agent(machine, index)?;
            }
        }
        Ok(harness)
    }

    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    fn start_agent(&mut self, machine: Machine, index: usize) -> anyhow::Result<()> {
        let slot = &mut self.agents[machine][index];
        let stdout = slot.start()?;
        let name = slot.name.clone();
        if let Some(stdout) = stdout {
            tokio::spawn(collect(name, stdout, Arc::clone(&self.acks)));
        }
        Ok(())
    }

    fn slot(&mut self, target: Target) -> &mut Slot {
        match target {
            Target::Server => &mut self.server,
            Target::Daemon { machine } => &mut self.daemons[machine - 1],
            Target::Client { machine, index } => &mut self.agents[machine][index],
        }
    }

    /// Waits until every agent has exited. Agents that exited with an error
    /// and agents still running at the deadline are both failures: the
    /// second is how a fault that hangs the system shows up.
    pub async fn wait_for_agents(&mut self, deadline: Duration) -> anyhow::Result<()> {
        let give_up = tokio::time::Instant::now() + deadline;
        loop {
            let running: Vec<String> = self
                .agents
                .iter_mut()
                .flatten()
                .filter_map(|slot| slot.running().then(|| slot.name.clone()))
                .collect();
            if running.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= give_up {
                bail!("agents still running after {deadline:?}: {}", running.join(", "));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        for slot in self.agents.iter_mut().flatten() {
            match slot.exited() {
                Some(status) if status.success() => {}
                status => bail!("{} ended with {status:?}", slot.name),
            }
        }
        Ok(())
    }

    /// Daemons and the server must still be running when the run ends: one
    /// that exited by itself crashed under the faults.
    pub fn crashed(&mut self) -> Vec<String> {
        let mut crashed = Vec::new();
        for slot in std::iter::once(&mut self.server).chain(self.daemons.iter_mut()) {
            if let Some(status) = slot.exited() {
                crashed.push(format!("{} exited with {status}", slot.name));
            }
        }
        crashed
    }

    pub async fn stop(&mut self) {
        for slot in self.agents.iter_mut().flatten() {
            slot.stop().await;
        }
        for slot in &mut self.daemons {
            slot.stop().await;
        }
        self.server.stop().await;
    }

    /// Removes every machine credential the drill enrolled.
    pub fn forget_machines(&self, layout: &Layout) -> Vec<String> {
        (1..=self.daemons.len())
            .filter_map(|machine| enroll::forget(&layout.machine_dir(machine)).err())
            .map(|err| format!("{err:#}"))
            .collect()
    }

    pub fn counts(&self) -> serde_json::Value {
        let all = std::iter::once(&self.server)
            .chain(&self.daemons)
            .chain(self.agents.iter().flatten());
        all.map(|slot| (slot.name.clone(), serde_json::json!({"starts": slot.starts, "kills": slot.kills})))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
}

impl Faults for Harness {
    async fn kill(&mut self, target: Target) -> anyhow::Result<()> {
        self.slot(target).kill().await
    }

    async fn restart(&mut self, target: Target) -> anyhow::Result<()> {
        match target {
            Target::Client { machine, index } => self.start_agent(machine, index),
            _ => {
                self.slot(target).start()?;
                Ok(())
            }
        }
    }

    fn cut(&mut self, machine: Machine, cut: Cut) {
        self.proxies[machine - 1].cut(cut);
    }

    fn heal(&mut self, machine: Machine) {
        self.proxies[machine - 1].heal();
    }
}

fn server_recipe(spec: &Spec, listen: SocketAddr) -> crate::process::Recipe {
    let bin = spec.server_bin.clone();
    let dir = spec.layout.server_dir();
    let endpoint = spec.layout.endpoint(0);
    Box::new(move || {
        let mut command = Command::new(&bin);
        command
            .arg("--data-dir")
            .arg(&dir)
            .args(["--endpoint", &endpoint, "--listen", &listen.to_string()]);
        command
    })
}

/// The daemon reads its server and credential from the `upstream.json`
/// enrollment wrote, as a daemon a client starts with no flags does.
fn daemon_recipe(spec: &Spec, machine: Machine) -> crate::process::Recipe {
    let bin = spec.server_bin.clone();
    let dir = spec.layout.machine_dir(machine);
    let endpoint = spec.layout.endpoint(machine);
    Box::new(move || {
        let mut command = Command::new(&bin);
        command
            .arg("--data-dir")
            .arg(&dir)
            .args(["--endpoint", &endpoint]);
        command
    })
}

fn agent_slots(spec: &Spec) -> anyhow::Result<Vec<Vec<Slot>>> {
    let me = std::env::current_exe()?;
    let mut machines = Vec::new();
    for machine in 0..spec.machines {
        let mut slots = Vec::new();
        for index in 0..spec.agents_per_machine {
            let unit = setup::unit(machine, index);
            let mut args = vec![
                "agent".to_owned(),
                "--endpoint".into(),
                spec.layout.endpoint(machine),
                "--name".into(),
                format!("{}-{unit}", spec.layout.tag),
                "--unit".into(),
                unit.clone(),
                "--lands".into(),
                spec.lands.to_string(),
            ];
            if let Some((m, i, at)) = spec.culprit {
                if (m, i) == (machine, index) {
                    args.extend(["--fail-at".into(), at.to_string()]);
                }
            }
            let me = me.clone();
            let log = spec.layout.root.join("logs").join(format!("{unit}.log"));
            let recipe: crate::process::Recipe = Box::new(move || {
                let mut command = Command::new(&me);
                command.args(&args);
                command
            });
            slots.push(Slot::new(unit, recipe).log_to(log));
        }
        machines.push(slots);
    }
    Ok(machines)
}

/// Waits for a freshly started daemon to accept connections.
async fn wait_ready(endpoint: &str) -> anyhow::Result<()> {
    let endpoint = rosen_protocol::Endpoint::new(endpoint);
    for _ in 0..150 {
        if rosen_client::Client::connect(&endpoint).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow::anyhow!("no daemon on {endpoint} after 15 s")).context("starting the drill")
}

async fn collect(agent: String, stdout: ChildStdout, acks: Acks) {
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(ack) = serde_json::from_str::<Ack>(&line) {
            acks.lock().expect("acks lock").push((agent.clone(), ack));
        }
    }
}
