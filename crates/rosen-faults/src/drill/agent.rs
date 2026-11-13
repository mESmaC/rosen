//! One agent of the drill, run as its own process so the plan can kill it.
//!
//! The agent lands `lands` changes, each writing one file under its own
//! directory. It keeps no state of its own: after a kill it asks the server
//! how many of its changes landed and resumes its newest open change, as a
//! real agent recovering from a crash would. Each acknowledged write and land
//! is printed as one JSON line; the drill checks every one survived.

use crate::failing;
use anyhow::{bail, Context};
use rosen_client::Client;
use rosen_core::{AgentId, ChangeState};
use rosen_protocol::{
    ApproveRequest, ChangeFilter, ChangeRecord, Endpoint, ErrorCode, LandStatus, LogRequest,
    NewChange, ReadRequest, Request, Response, SubmitReceipt, WorkspaceBytes, WorkspaceWrite,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::time::Duration;

/// Longer than any planned partition, so a request only times out when a
/// fault left it stranded rather than merely delayed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, clap::Args)]
pub struct AgentArgs {
    /// The daemon endpoint of this agent's machine.
    #[arg(long)]
    pub endpoint: String,
    /// The agent's identity, which owns its changes.
    #[arg(long)]
    pub name: String,
    /// The directory this agent writes, its own build unit.
    #[arg(long)]
    pub unit: String,
    /// How many changes to land.
    #[arg(long)]
    pub lands: usize,
    /// Land the injected failing commit as this land (0-based).
    #[arg(long)]
    pub fail_at: Option<usize>,
}

/// What an agent reports on stdout, one JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ack", rename_all = "snake_case")]
pub enum Ack {
    /// The server stored `path` with `data` in `change`.
    Wrote {
        change: String,
        path: String,
        data: String,
    },
    /// The land of `change` was acknowledged as `commit`.
    Landed { change: String, commit: String },
    /// The change carries the injected failing commit.
    Culprit { change: String },
}

fn report(ack: &Ack) {
    // One write per line keeps lines whole even when the process is killed.
    println!("{}", serde_json::to_string(ack).expect("acks serialize"));
}

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    let agent = AgentId::new(&args.name).map_err(|err| anyhow::anyhow!("{err}"))?;
    let endpoint = Endpoint::new(&args.endpoint);
    loop {
        match round(&args, &agent, &endpoint).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            // Every fault surfaces here: a dead daemon, a reset link, or a
            // request stranded by a silent partition. Start over.
            Err(err) => {
                eprintln!("{}: {err:#}; reconnecting", args.name);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

/// Lands one change. True once every land is done.
async fn round(args: &AgentArgs, agent: &AgentId, endpoint: &Endpoint) -> anyhow::Result<bool> {
    let client = timed(Client::connect(endpoint)).await?;
    let landed = mine(&client, agent, ChangeState::Landed).await?.len();
    if landed >= args.lands {
        return Ok(true);
    }
    let failure_landed = args.fail_at.is_some() && failure_landed(&client, args).await?;
    let change = match resumable(&client, args, agent, failure_landed).await? {
        Some(change) => change,
        None => new_change(&client, agent).await?,
    };
    let path = format!("{}/{change}.rs", args.unit);
    write(&client, &change, &path, format!("// change {change}\n")).await?;
    // A server revert reopens the culprit and lowers the landed count, so
    // the count alone would inject the failure into a second change.
    if args.fail_at == Some(landed) && !failure_landed {
        write(&client, &change, &failure_path(args), failing::contents(&change)).await?;
        report(&Ack::Culprit { change: change.clone() });
    }
    land(&client, agent, &change).await?;
    Ok(false)
}

/// The newest unlanded change to carry on with after a restart: open, or
/// ready when a kill cut its land short. Once the failure has landed, an
/// open change that holds the failure file is the culprit the server
/// reverted and reopened: its owner would fix it, so the drill leaves it as
/// R12 left it. Before that, it is the culprit a kill interrupted, and the
/// agent carries on landing it.
async fn resumable(
    client: &Client,
    args: &AgentArgs,
    agent: &AgentId,
    failure_landed: bool,
) -> anyhow::Result<Option<String>> {
    let mut open = mine(client, agent, ChangeState::Open).await?;
    open.extend(mine(client, agent, ChangeState::Ready).await?);
    open.sort_by_key(|record| record.created_at);
    while let Some(record) = open.pop() {
        let change = record.change.id.to_string();
        if !failure_landed || !holds_failure(client, args, &change).await? {
            return Ok(Some(change));
        }
    }
    Ok(None)
}

/// Whether the base ever held this agent's failure file. Its land and its
/// server revert both touch it, so the base's path log is non-empty from
/// the culprit's land onwards, even after the revert removed the file.
async fn failure_landed(client: &Client, args: &AgentArgs) -> anyhow::Result<bool> {
    let request = Request::Log(LogRequest {
        at: None,
        paths: vec![failure_path(args)],
        limit: Some(1),
    });
    match call(client, request).await? {
        Response::Log(log) => Ok(!log.entries.is_empty()),
        other => bail!("unexpected answer to log: {other:?}"),
    }
}

fn failure_path(args: &AgentArgs) -> String {
    format!("{}/{}", args.unit, failing::FILE_NAME)
}

async fn holds_failure(client: &Client, args: &AgentArgs, change: &str) -> anyhow::Result<bool> {
    let request = Request::ReadFile(ReadRequest {
        at: Some(change.into()),
        path: failure_path(args),
    });
    match timed(client.request(request)).await {
        Ok(_) => Ok(true),
        Err(err) => match err.downcast_ref::<rosen_client::Error>() {
            Some(rosen_client::Error::Server(body)) if body.code == ErrorCode::NotFound => Ok(false),
            _ => Err(err),
        },
    }
}

async fn call(client: &Client, request: Request) -> anyhow::Result<Response> {
    timed(client.request(request)).await
}

async fn timed<T, E>(work: impl Future<Output = Result<T, E>>) -> anyhow::Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(REQUEST_TIMEOUT, work).await {
        Ok(result) => Ok(result?),
        Err(_) => bail!("no answer in {REQUEST_TIMEOUT:?}"),
    }
}

async fn mine(client: &Client, agent: &AgentId, state: ChangeState) -> anyhow::Result<Vec<ChangeRecord>> {
    let filter = ChangeFilter {
        owner: Some(agent.clone()),
        state: Some(state),
    };
    match call(client, Request::ChangeList(filter)).await? {
        Response::Changes(records) => Ok(records),
        other => bail!("unexpected answer to change list: {other:?}"),
    }
}

async fn new_change(client: &Client, agent: &AgentId) -> anyhow::Result<String> {
    let request = Request::ChangeNew(NewChange {
        owner: agent.clone(),
        on: None,
    });
    match call(client, request).await? {
        Response::Change(record) => Ok(record.change.id.to_string()),
        other => bail!("unexpected answer to change new: {other:?}"),
    }
}

/// Writes a file; repeating the same write after a reconnect stores nothing
/// new. The ack is printed only once the server answered.
async fn write(client: &Client, change: &str, path: &str, data: String) -> anyhow::Result<()> {
    let request = Request::WorkspaceWrite(WorkspaceWrite {
        change: change.into(),
        path: path.into(),
        data: Some(WorkspaceBytes(data.clone().into_bytes())),
    });
    call(client, request).await?;
    report(&Ack::Wrote {
        change: change.into(),
        path: path.into(),
        data,
    });
    Ok(())
}

/// Approves, submits receipts for whichever base the server names, and
/// lands, until the server says the change is on the base.
async fn land(client: &Client, agent: &AgentId, change: &str) -> anyhow::Result<()> {
    let head = show(client, change).await?.head;
    approve(client, agent, change, head).await?;
    for _ in 0..1000 {
        let request = Request::Land {
            change: change.into(),
        };
        let Response::Land(answer) = call(client, request).await? else {
            bail!("unexpected answer to land");
        };
        match answer.outcome {
            LandStatus::Landed { commit, .. } | LandStatus::AlreadyLanded { commit } => {
                report(&Ack::Landed {
                    change: change.into(),
                    commit: commit.to_string(),
                });
                return Ok(());
            }
            LandStatus::Refused { reason, .. } => {
                // A server rebase can move the head between attempts.
                let head = show(client, change).await?.head;
                match reason.as_str() {
                    "no_receipt" | "needs_recheck" | "receipt_stale" => {
                        receipt(client, agent, change, head, answer.base).await?;
                    }
                    "unapproved" => approve(client, agent, change, head).await?,
                    _ => bail!("land of {change} refused: {reason}"),
                }
            }
        }
    }
    bail!("{change} did not land after 1000 attempts")
}

async fn show(client: &Client, change: &str) -> anyhow::Result<rosen_core::Change> {
    let request = Request::ChangeShow {
        change: change.into(),
    };
    match call(client, request).await? {
        Response::Change(record) => Ok(record.change),
        other => bail!("unexpected answer to change show: {other:?}"),
    }
}

async fn approve(
    client: &Client,
    agent: &AgentId,
    change: &str,
    head: rosen_core::ObjectId,
) -> anyhow::Result<()> {
    // The drill stands in for review: each agent has its own reviewer.
    let reviewer = AgentId::new(format!("{agent}-reviewer")).map_err(|err| anyhow::anyhow!("{err}"))?;
    let request = Request::ReviewApprove(ApproveRequest {
        change: change.into(),
        reviewer,
        head,
    });
    call(client, request).await.context("approve")?;
    Ok(())
}

async fn receipt(
    client: &Client,
    agent: &AgentId,
    change: &str,
    head: rosen_core::ObjectId,
    base: rosen_core::ObjectId,
) -> anyhow::Result<()> {
    // The drill stands in for the build queue: the checks always pass
    // before landing; the injected failure is one only post-land tests see.
    let submitter = AgentId::new(format!("{agent}-checker")).map_err(|err| anyhow::anyhow!("{err}"))?;
    let request = Request::ReceiptSubmit(SubmitReceipt {
        change: change.into(),
        head,
        base,
        check_set: "pre-land".into(),
        submitter,
    });
    call(client, request).await.context("receipt")?;
    Ok(())
}
