//! `rosen change new|list|show|abandon` (SPEC R2, R10).

use crate::commands::ChangeCommand;
use crate::server::Succeeded;
use rosen_client::protocol::{ChangeFilter, ChangeRecord, NewChange};
use rosen_client::{Endpoint, Error};
use rosen_core::{AgentId, ChangeParent, ChangeState};
use serde_json::json;

/// Schema tags for `--json` output (SPEC R10). Bump on any shape change.
const CHANGE_SCHEMA: &str = "rosen.change/v1";
const CHANGE_LIST_SCHEMA: &str = "rosen.change.list/v1";

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    command: &ChangeCommand,
) -> Result<Succeeded, Error> {
    // Parse arguments before starting a daemon, so a typo costs nothing.
    let request = ChangeCall::parse(command)?;
    let client = rosen_client::connect_or_start(endpoint).await?;
    match request {
        ChangeCall::New(new) => print_change(json, &client.change_new(new).await?),
        ChangeCall::List(filter) => print_list(json, &client.change_list(filter).await?),
        ChangeCall::Show(change) => print_change(json, &client.change_show(&change).await?),
        ChangeCall::Abandon(change) => print_change(json, &client.change_abandon(&change).await?),
    }
    Ok(true)
}

enum ChangeCall {
    New(NewChange),
    List(ChangeFilter),
    Show(String),
    Abandon(String),
}

impl ChangeCall {
    fn parse(command: &ChangeCommand) -> Result<Self, Error> {
        Ok(match command {
            ChangeCommand::New { on, owner } => ChangeCall::New(NewChange {
                owner: resolve_owner(owner.as_deref())?,
                on: on.clone(),
            }),
            ChangeCommand::List { owner, state } => ChangeCall::List(ChangeFilter {
                owner: owner.as_deref().map(agent).transpose()?,
                state: state.as_deref().map(parse_state).transpose()?,
            }),
            ChangeCommand::Show { change } => ChangeCall::Show(change.clone()),
            ChangeCommand::Abandon { change } => ChangeCall::Abandon(change.clone()),
        })
    }
}

/// The owner is the agent's identity. Velocity sets ROSEN_AGENT per thread; a
/// human at a terminal falls back to their OS user name.
fn resolve_owner(explicit: Option<&str>) -> Result<AgentId, Error> {
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
