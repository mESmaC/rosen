//! `rosen change new|list|show|abandon` (SPEC R2, R10).

use crate::commands::ChangeCommand;
use crate::server::Succeeded;
use rosen_client::protocol::{ChangeFilter, ChangeRecord, NewChange};
use rosen_client::workspace::{Options, Report, Sparse};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::{AgentId, ChangeParent, ChangeState};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::output::CHANGE_LIST_SCHEMA;
use crate::output::CHANGE_SCHEMA;
use crate::output::CHANGE_WORKSPACE_SCHEMA;

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    command: &ChangeCommand,
) -> Result<Succeeded, Error> {
    // Parse arguments before starting a daemon, so a typo costs nothing.
    let request = ChangeCall::parse(command)?;
    let client = rosen_client::connect_or_start(endpoint).await?;
    match request {
        ChangeCall::New(new, None) => print_change(json, &client.change_new(new).await?),
        ChangeCall::New(new, Some(workspace)) => {
            let record = client.change_new(new).await?;
            new_workspace(json, &client, &record, workspace).await?;
        }
        ChangeCall::List(filter) => print_list(json, &client.change_list(filter).await?),
        ChangeCall::Show(change) => print_change(json, &client.change_show(&change).await?),
        ChangeCall::Abandon(change) => print_change(json, &client.change_abandon(&change).await?),
    }
    Ok(true)
}

enum ChangeCall {
    New(NewChange, Option<WorkspaceCall>),
    List(ChangeFilter),
    Show(String),
    Abandon(String),
}

impl ChangeCall {
    fn parse(command: &ChangeCommand) -> Result<Self, Error> {
        Ok(match command {
            ChangeCommand::New {
                on,
                owner,
                workspace,
                sparse,
            } => ChangeCall::New(
                NewChange {
                    owner: resolve_owner(owner.as_deref())?,
                    on: on.clone(),
                },
                workspace
                    .as_ref()
                    .map(|dir| WorkspaceCall::parse(dir, sparse))
                    .transpose()?,
            ),
            ChangeCommand::List { owner, state } => ChangeCall::List(ChangeFilter {
                owner: owner.as_deref().map(agent).transpose()?,
                state: state.as_deref().map(parse_state).transpose()?,
            }),
            ChangeCommand::Show { change } => ChangeCall::Show(change.clone()),
            ChangeCommand::Abandon { change } => ChangeCall::Abandon(change.clone()),
        })
    }
}

/// Where and how `change new --workspace` writes the change's files.
struct WorkspaceCall {
    dir: PathBuf,
    options: Options,
}

impl WorkspaceCall {
    fn parse(dir: &Path, sparse: &[String]) -> Result<Self, Error> {
        let sparse =
            Sparse::paths(sparse).map_err(|err| Error::InvalidArgument(err.to_string()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            options: Options {
                sparse,
                source: None,
            },
        })
    }
}

/// Materialises a new change's workspace (SPEC R3). When that fails the
/// change still exists, so its ID is printed before the error.
async fn new_workspace(
    json: bool,
    client: &Client,
    record: &ChangeRecord,
    call: WorkspaceCall,
) -> Result<(), Error> {
    let report = match client
        .materialise_change(record, &call.dir, call.options)
        .await
    {
        Ok(report) => report,
        Err(err) => {
            eprintln!("created change {} but not its workspace", record.change.id);
            return Err(err);
        }
    };
    if json {
        let body = json!({
            "schema": CHANGE_WORKSPACE_SCHEMA,
            "change": record,
            "workspace": workspace_json(&call.dir, &report),
        });
        println!("{body}");
    } else {
        print_change(false, record);
        println!(
            "workspace: {} ({}, {} files)",
            call.dir.display(),
            report.method.as_str(),
            report.files
        );
        if let Some(reason) = &report.fallback {
            println!("note:      not block cloned: {reason}");
        }
    }
    Ok(())
}

fn workspace_json(dir: &Path, report: &Report) -> serde_json::Value {
    json!({
        "path": dir.display().to_string(),
        "method": report.method.as_str(),
        "files": report.files,
        "cloned": report.cloned,
        "copied": report.copied,
        "written": report.written,
        "bytes_written": report.bytes_written,
        "fallback": report.fallback,
    })
}

/// The owner is the agent's identity. Velocity sets ROSEN_AGENT per thread; a
/// human at a terminal falls back to their OS user name.
pub(crate) fn resolve_owner(explicit: Option<&str>) -> Result<AgentId, Error> {
    let from_env = || {
        ["ROSEN_AGENT", "USERNAME", "USER"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    };
    let name = explicit
        .map(str::to_owned)
        .or_else(from_env)
        .ok_or_else(|| {
            Error::InvalidArgument("no owner: pass --owner or set ROSEN_AGENT".into())
        })?;
    agent(&name)
}

fn agent(name: &str) -> Result<AgentId, Error> {
    AgentId::new(name).map_err(|err| Error::InvalidArgument(err.to_string()))
}

fn parse_state(name: &str) -> Result<ChangeState, Error> {
    name.parse()
        .map_err(|err: rosen_core::Error| Error::InvalidArgument(err.to_string()))
}

fn parent_text(parent: &ChangeParent) -> String {
    match parent {
        ChangeParent::Base(commit) => format!("base {commit}"),
        ChangeParent::Change(change) => format!("change {change}"),
    }
}

fn print_change(json: bool, record: &ChangeRecord) {
    if json {
        let body = json!({ "schema": CHANGE_SCHEMA, "change": record });
        println!("{body}");
        return;
    }
    let change = &record.change;
    println!("change:    {}", change.id);
    println!("owner:     {}", change.owner);
    println!("state:     {}", change.state);
    println!("parent:    {}", parent_text(&change.parent));
    println!("head:      {}", change.head);
    println!("snapshots: {}", change.snapshots.len());
    println!("conflicts: {}", change.conflicts.len());
}

fn print_list(json: bool, records: &[ChangeRecord]) {
    if json {
        let body = json!({ "schema": CHANGE_LIST_SCHEMA, "changes": records });
        println!("{body}");
        return;
    }
    for record in records {
        let change = &record.change;
        println!(
            "{}  {:<9}  {}  {}",
            change.id,
            change.state.as_str(),
            change.owner,
            parent_text(&change.parent)
        );
    }
}
