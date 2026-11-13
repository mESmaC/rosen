//! `rosen-stress agents`: runs this machine's share of the agents against
//! its own daemon, which is the server on the serving machine and the
//! tier-1 cache in front of it everywhere else (D8).

use crate::agent::Agent;
use crate::claims::HotClaims;
use crate::plan::Plan;
use crate::report::Report;
use anyhow::{Context, Result};
use rosen_client::protocol::LogRequest;
use rosen_client::{AgentId, Client, Endpoint, ObjectId, Session};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Everything this machine's agents share.
pub struct Shared {
    pub plan: Plan,
    pub endpoint: Endpoint,
    pub work_dir: PathBuf,
    pub reviewer: AgentId,
    pub checker: AgentId,
    pub hot: HotClaims,
    report: Mutex<Report>,
}

impl Shared {
    pub fn record(&self, update: impl FnOnce(&mut Report)) {
        update(&mut self.report.lock().expect("report lock"));
    }
}

/// This machine's part of the run.
#[derive(Debug, Clone)]
pub struct MachineRun {
    pub machine: String,
    pub first_agent: usize,
    pub agent_count: usize,
    pub endpoint: Endpoint,
    /// Where workspaces are made. A Dev Drive gives block clones (R3).
    pub work_dir: PathBuf,
    /// Ready workspaces to ask the daemon for, as Velocity raises the pool
    /// for a launch (R3); 0 leaves the daemon's size alone.
    pub pool_size: usize,
    /// How long to wait for the pool to fill before the agents start.
    pub pool_wait: Duration,
}

pub async fn run_machine(plan: &Plan, run: &MachineRun) -> Result<Report> {
    let shared = Arc::new(Shared {
        plan: plan.clone(),
        endpoint: run.endpoint.clone(),
        work_dir: std::path::absolute(&run.work_dir)?,
        reviewer: AgentId::new(plan.reviewer_name(&run.machine))?,
        checker: AgentId::new(plan.checker_name(&run.machine))?,
        hot: HotClaims::default(),
        report: Mutex::new(Report {
            plan: Some(plan.clone()),
            machine: run.machine.clone(),
            first_agent: run.first_agent,
            agent_count: run.agent_count,
            ..Report::default()
        }),
    });
    let control = Client::connect(&run.endpoint)
        .await
        .with_context(|| format!("connecting to the daemon on {}", run.endpoint))?;
    if run.pool_size > 0 {
        fill_pool(&control, run.pool_size, run.pool_wait).await?;
    }
    let base = base_tip(&control).await?;
    shared.record(|report| report.base_at_start = Some(base));
    let agents = open_agents(&shared, run).await?;
    let started = Instant::now();
    let tasks: Vec<_> = agents
        .into_iter()
        .map(|agent| tokio::spawn(agent.run()))
        .collect();
    for task in tasks {
        task.await.context("an agent task panicked")?;
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    shared.record(|report| report.elapsed_ms = elapsed_ms);
    let mut report = shared.report.lock().expect("report lock").clone();
    report.workspaces_kept = workspaces_left(&report).await;
    Ok(report)
}

/// How long the daemon gets to delete closed changes' workspaces after the
/// last land, since it does so on change events, off the land's path.
const DELETION_GRACE: Duration = Duration::from_secs(30);

/// Every workspace still on disk once the grace period ends, though its
/// change is closed and R4 deletes it automatically.
async fn workspaces_left(report: &Report) -> Vec<String> {
    let deadline = Instant::now() + DELETION_GRACE;
    loop {
        let left: Vec<String> = report
            .changes
            .iter()
            .filter(|change| change.workspace.exists())
            .map(|change| format!("{} at {}", change.change, change.workspace.display()))
            .collect();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// One connection per agent, opened before any starts, so connection
/// set-up never counts towards a workspace start.
async fn open_agents(shared: &Arc<Shared>, run: &MachineRun) -> Result<Vec<Agent>> {
    let mut agents = Vec::with_capacity(run.agent_count);
    for index in run.first_agent..run.first_agent + run.agent_count {
        let name = AgentId::new(shared.plan.agent_name(index))?;
        let session = Session::builder()
            .endpoint(run.endpoint.clone())
            .agent(name.clone())
            .start_daemon(false)
            .open()
            .await?;
        agents.push(Agent {
            index,
            name,
            session,
            shared: Arc::clone(shared),
        });
    }
    Ok(agents)
}

pub async fn base_tip(client: &Client) -> Result<ObjectId> {
    let request = LogRequest {
        at: None,
        paths: Vec::new(),
        limit: Some(1),
    };
    let log = client.log(request).await?;
    let tip = log
        .entries
        .first()
        .context("the base has no commits; import the seed first")?;
    Ok(tip.commit)
}

/// Sets the pool target and waits until that many ready workspaces exist,
/// so starts are measured "with the warm pool filled" (§5).
async fn fill_pool(client: &Client, size: usize, wait: Duration) -> Result<()> {
    client.set_pool_size(size).await?;
    let pool_dir = PathBuf::from(client.status().await?.data_dir).join("warm-pool");
    let deadline = Instant::now() + wait;
    loop {
        let ready = count_ready(&pool_dir);
        if ready >= size {
            return Ok(());
        }
        if Instant::now() >= deadline {
            eprintln!(
                "rosen-stress: warm pool has {ready} of {size} after {wait:?}; starting anyway"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Ready slots are the pool directory's visible subdirectories; staging
/// directories are hidden until complete (R3).
fn count_ready(pool_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(pool_dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .count()
}
