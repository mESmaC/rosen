//! `rosen-stress local`: the whole run on one machine. It seeds a
//! repository, starts a private server, imports the seed, runs every agent
//! and verifies. The three-machine run (§5) uses `seed`, `agents` and
//! `verify` instead; this is the same path with one machine.

use crate::machine::{run_machine, MachineRun};
use crate::plan::Plan;
use crate::report::Report;
use crate::verify::{verify, Failures};
use anyhow::{bail, Context, Result};
use rosen_client::{Client, Endpoint, Error};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct LocalRun {
    /// A new directory for the seed, the server's data, the workspaces,
    /// the server log and the report.
    pub dir: PathBuf,
    pub pool_size: usize,
    pub pool_wait: Duration,
}

pub async fn run_local(plan: &Plan, local: &LocalRun) -> Result<(Report, Failures)> {
    if local.dir.exists() {
        bail!(
            "{} already exists; give a new directory",
            local.dir.display()
        );
    }
    std::fs::create_dir_all(&local.dir)?;
    let dir = std::path::absolute(&local.dir)?;
    let seed = dir.join("seed");
    crate::seed::write_seed(plan, &seed)?;
    let endpoint = private_endpoint(&dir);
    let mut server = start_server(&dir, &endpoint)?;
    let result = run_against(plan, local, &dir, &seed, &endpoint).await;
    if let Ok(client) = Client::connect(&endpoint).await {
        let _ = client.shutdown().await;
    }
    if tokio::time::timeout(Duration::from_secs(10), wait_exit(&mut server))
        .await
        .is_err()
    {
        let _ = server.kill();
    }
    result
}

async fn run_against(
    plan: &Plan,
    local: &LocalRun,
    dir: &Path,
    seed: &Path,
    endpoint: &Endpoint,
) -> Result<(Report, Failures)> {
    let client = connect_when_up(endpoint).await?;
    let seed_path = seed.to_string_lossy();
    client
        .import(&seed_path, Some("main"), None)
        .await
        .context("importing the seed")?;
    let run = MachineRun {
        machine: "local".into(),
        first_agent: 0,
        agent_count: plan.agents,
        endpoint: endpoint.clone(),
        work_dir: dir.join("work"),
        pool_size: local.pool_size,
        pool_wait: local.pool_wait,
    };
    let report = run_machine(plan, &run).await?;
    std::fs::write(
        dir.join("report-local.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    // A fresh connection: the import's one sat idle through the whole run.
    let client = connect_when_up(endpoint).await?;
    let failures = verify(&client, std::slice::from_ref(&report)).await?;
    Ok((report, failures))
}

/// A pipe or socket only this run uses, so it never meets the user's own
/// daemon.
fn private_endpoint(dir: &Path) -> Endpoint {
    if cfg!(windows) {
        Endpoint::new(format!("rosen-stress-{}", std::process::id()))
    } else {
        Endpoint::new(dir.join("server.sock").to_string_lossy())
    }
}

fn start_server(dir: &Path, endpoint: &Endpoint) -> Result<Child> {
    let binary = rosen_client::daemon::server_binary();
    let log = std::fs::File::create(dir.join("server.log"))?;
    Command::new(&binary)
        .arg("--data-dir")
        .arg(dir.join("server"))
        .arg("--endpoint")
        .arg(endpoint.as_str())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))
}

async fn connect_when_up(endpoint: &Endpoint) -> Result<Client> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match Client::connect(endpoint).await {
            Err(Error::NotRunning(_)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            connected => return connected.context("connecting to the stress server"),
        }
    }
}

async fn wait_exit(child: &mut Child) {
    while matches!(child.try_wait(), Ok(None)) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
