//! `rosen-stress agents`: runs this machine's share of the agents against
//! its own daemon, which is the server on the serving machine and the
//! tier-1 cache in front of it everywhere else (D8).
//!
//! The run is journalled as it goes. Started again on the same journal
//! after a kill, it resumes every agent where it stopped (§5).

use crate::agent::Agent;
use crate::claims::HotClaims;
use crate::plan::Plan;
use crate::report::{Began, Journal, Record, Report};
use anyhow::{bail, Context, Result};
use rosen_client::protocol::LogRequest;
use rosen_client::{AgentId, Client, Endpoint, ObjectId, Session};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the machine waits for its daemon to answer when it starts,
/// longer than any outage a fault plan makes.
const DAEMON_WAIT: Duration = Duration::from_secs(120);

/// Everything this machine's agents share.
pub struct Shared {
    pub plan: Plan,
    pub endpoint: Endpoint,
    pub work_dir: PathBuf,
    pub reviewer: AgentId,
    pub checker: AgentId,
    pub hot: HotClaims,
    state: Mutex<(Report, Journal)>,
}

impl Shared {
    /// Journals one record, then folds it into the report.
    pub fn record(&self, record: Record) {
        let mut state = self.state.lock().expect("report lock");
        let (report, journal) = &mut *state;
        if let Err(err) = journal.append(&record) {
            eprintln!("rosen-stress: could not journal a record: {err}");
        }
        report.apply(&record);
    }
}

/// One agent's changes from earlier processes on this journal.
#[derive(Debug, Default, Clone)]
pub struct Progress {
    pub began: BTreeMap<usize, Began>,
    /// Finished change numbers, and whether each landed.
    pub finished: BTreeMap<usize, bool>,
}

fn progress_of(records: &[Record]) -> HashMap<usize, Progress> {
    let mut agents: HashMap<usize, Progress> = HashMap::new();
    for record in records {
        match record {
            Record::Began(began) => {
                let progress = agents.entry(began.index).or_default();
                progress.began.insert(began.n, began.clone());
            }
            Record::Outcome(outcome) => {
                let progress = agents.entry(outcome.index).or_default();
                progress.finished.insert(outcome.n, outcome.landed.is_some());
            }
            _ => {}
        }
    }
    agents
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
    /// Records as they happen; an existing journal is resumed.
    pub journal: PathBuf,
    /// Ready workspaces to ask the daemon for, as Velocity raises the pool
    /// for a launch (R3); 0 leaves the daemon's size alone.
    pub pool_size: usize,
    /// How long to wait for the pool to fill before the agents start.
    pub pool_wait: Duration,
}

pub async fn run_machine(plan: &Plan, run: &MachineRun) -> Result<Report> {
    let (journal, records) = Journal::open(&run.journal)?;
    let mut report = Report {
        plan: Some(plan.clone()),
        machine: run.machine.clone(),
        first_agent: run.first_agent,
        agent_count: run.agent_count,
        ..Report::default()
    };
    records.iter().for_each(|record| report.apply(record));
    let mut progress = progress_of(&records);
    let shared = Arc::new(Shared {
        plan: plan.clone(),
        endpoint: run.endpoint.clone(),
        work_dir: std::path::absolute(&run.work_dir)?,
        reviewer: AgentId::new(plan.reviewer_name(&run.machine))?,
        checker: AgentId::new(plan.checker_name(&run.machine))?,
        hot: HotClaims::default(),
        state: Mutex::new((report, journal)),
    });
    let control = connect_when_up(&run.endpoint, DAEMON_WAIT).await?;
    if run.pool_size > 0 && records.is_empty() {
        fill_pool(&control, run.pool_size, run.pool_wait).await?;
    } else if run.pool_size > 0 {
        // Resuming mid-run: the pool is in use, so waiting for it to be
        // full would only stall the agents.
        control.set_pool_size(run.pool_size).await?;
    }
    let base = base_tip(&control).await?;
    shared.record(Record::Started { base });
    let agents = open_agents(&shared, run, &mut progress).await?;
    let started = Instant::now();
    let tasks: Vec<_> = agents
        .into_iter()
        .map(|agent| tokio::spawn(agent.run()))
        .collect();
    for task in tasks {
        task.await.context("an agent task panicked")?;
    }
    let ms = started.elapsed().as_millis() as u64;
    shared.record(Record::Ran { ms });
    let mut report = shared.state.lock().expect("report lock").0.clone();
    report.workspaces_kept = workspaces_left(&report, plan).await;
    Ok(report)
}

/// Connects once the daemon answers, waiting through a restart or an
/// outage of the server behind it.
pub async fn connect_when_up(endpoint: &Endpoint, wait: Duration) -> Result<Client> {
    let deadline = Instant::now() + wait;
    loop {
        let tried = tokio::time::timeout(Duration::from_secs(10), Client::connect(endpoint)).await;
        if let Ok(Ok(client)) = tried {
            if client.ping().await.is_ok() {
                return Ok(client);
            }
        }
        if Instant::now() >= deadline {
            bail!("no daemon answered on {endpoint} within {wait:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// How long the daemon gets to delete closed changes' workspaces after the
/// last land, since it does so on change events, off the land's path.
const DELETION_GRACE: Duration = Duration::from_secs(30);

/// Every workspace still on disk once the grace period ends, though its
/// change is closed and R4 deletes it automatically. The culprit is left
/// out: its revert reopens it (R12), so whether its workspace survives
/// depends on whether the reopen beat the deletion.
async fn workspaces_left(report: &Report, plan: &Plan) -> Vec<String> {
    let deadline = Instant::now() + DELETION_GRACE;
    let culprit = plan.culprit();
    loop {
        let left: Vec<String> = report
            .changes
            .iter()
            .filter(|change| Some((change.index, change.n)) != culprit)
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
async fn open_agents(
    shared: &Arc<Shared>,
    run: &MachineRun,
    progress: &mut HashMap<usize, Progress>,
) -> Result<Vec<Agent>> {
    let mut agents = Vec::with_capacity(run.agent_count);
    for index in run.first_agent..run.first_agent + run.agent_count {
        let name = AgentId::new(shared.plan.agent_name(index))?;
        let session = open_session(&run.endpoint, &name).await?;
        agents.push(Agent {
            index,
            name,
            session,
            shared: Arc::clone(shared),
            progress: progress.remove(&index).unwrap_or_default(),
        });
    }
    Ok(agents)
}

async fn open_session(endpoint: &Endpoint, name: &AgentId) -> Result<Session> {
    let deadline = Instant::now() + DAEMON_WAIT;
    loop {
        let opened = Session::builder()
            .endpoint(endpoint.clone())
            .agent(name.clone())
            .start_daemon(false)
            .open()
            .await;
        match opened {
            Ok(session) => return Ok(session),
            Err(err) if Instant::now() >= deadline => {
                return Err(err).with_context(|| format!("opening a session for {name}"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// The base tip, read through the daemon. Retried, since a cache daemon
/// cut off from the server cannot answer until the outage ends.
pub async fn base_tip(client: &Client) -> Result<ObjectId> {
    let deadline = Instant::now() + DAEMON_WAIT;
    let request = LogRequest {
        at: None,
        paths: Vec::new(),
        limit: Some(1),
    };
    let log = loop {
        match client.log(request.clone()).await {
            Ok(log) => break log,
            Err(err) if Instant::now() >= deadline => return Err(err.into()),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
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
