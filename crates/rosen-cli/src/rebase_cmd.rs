//! `rosen rebase` (SPEC R2, R5, R10): moves a change onto the current base
//! and, run inside its workspace, writes the rebased files there, with
//! conflict markers where the merge conflicted.

use std::path::Path;

use crate::output::REBASE_SCHEMA;
use crate::server::Succeeded;
use rosen_client::protocol::ChangeRecord;
use rosen_client::snapshot::WorkspaceMarker;
use rosen_client::{Client, Endpoint, Error};
use rosen_core::ChangeParent;
use serde_json::json;

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    change: Option<&str>,
) -> Result<Succeeded, Error> {
    let cwd = std::env::current_dir()?;
    let Some(root) = WorkspaceMarker::find_root(&cwd).filter(|_| change.is_none()) else {
        // Checked before connecting, so a bad command line never starts a server.
        let change = crate::claim_cmd::change_of(change)?;
        let client = rosen_client::connect_or_start(endpoint).await?;
        let record = client.rebase_change(&change).await?;
        let conflicts = conflict_paths(&client, &record).await;
        print_answer(json, &record, None, &conflicts);
        return Ok(true);
    };
    let owner = crate::change_cmd::resolve_owner(None)?;
    let client = rosen_client::connect_or_start(endpoint).await?;
    // The server rebases the stored head, so the workspace's edits go first;
    // applying refuses any edit that is not stored.
    let report = client.snapshot_workspace(&root, None).await?;
    let record = client.rebase_change(&report.change.to_string()).await?;
    client.apply_rebase(&root, &owner).await?;
    let conflicts = conflict_paths(&client, &record).await;
    print_answer(json, &record, Some(&root), &conflicts);
    Ok(true)
}

async fn conflict_paths(client: &Client, record: &ChangeRecord) -> Vec<String> {
    if record.change.conflicts.is_empty() {
        return Vec::new();
    }
    let gates = client.land_check(&record.change.id.to_string()).await.ok();
    crate::status_next::stored_conflict_paths(gates.as_ref())
}

fn print_answer(
    json: bool,
    record: &ChangeRecord,
    workspace: Option<&Path>,
    conflict_paths: &[String],
) {
    let change = &record.change;
    let managed = crate::managed::is_managed();
    let commands = if change.conflicts.is_empty() {
        Vec::new()
    } else {
        crate::managed::conflict_commands(
            conflict_paths,
            Some(workspace.is_some()),
            crate::status_next::was_named(record),
            &change.id.to_string(),
            managed,
        )
    };
    if json {
        let mut rebase = json!({ "change": record, "workspace": workspace });
        if !change.conflicts.is_empty() {
            rebase["conflict_paths"] = json!(conflict_paths);
            rebase["next_commands"] = json!(commands);
        }
        println!("{}", json!({ "schema": REBASE_SCHEMA, "rebase": rebase }));
        return;
    }
    match change.parent {
        ChangeParent::Base(commit) => println!("rebased change {} onto {commit}", change.id),
        ChangeParent::Change(below) => println!("change {} stays on change {below}", change.id),
    }
    if let Some(root) = workspace {
        println!("wrote its head {} into {}", change.head, root.display());
    }
    if !change.conflicts.is_empty() {
        print_conflicts(change.conflicts.len(), conflict_paths, &commands);
    }
}

fn print_conflicts(count: usize, paths: &[String], commands: &[String]) {
    if paths.is_empty() {
        println!("{count} open conflict(s):");
    } else {
        println!("{count} open conflict(s) in:");
        for path in paths {
            println!("  {path}");
        }
    }
    println!("Combine both sides between the conflict markers, then run:");
    for cmd in commands {
        println!("  {cmd}");
    }
}
