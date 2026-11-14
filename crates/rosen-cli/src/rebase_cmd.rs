//! `rosen rebase` (SPEC R2, R5, R10): moves a change onto the current base
//! and, run inside its workspace, writes the rebased files there, with
//! conflict markers where the merge conflicted.

use std::path::Path;

use crate::output::REBASE_SCHEMA;
use crate::server::Succeeded;
use rosen_client::protocol::ChangeRecord;
use rosen_client::snapshot::WorkspaceMarker;
use rosen_client::{Endpoint, Error};
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
        print_answer(json, &record, None);
        return Ok(true);
    };
    let owner = crate::change_cmd::resolve_owner(None)?;
    let client = rosen_client::connect_or_start(endpoint).await?;
    // The server rebases the stored head, so the workspace's edits go first;
    // applying refuses any edit that is not stored.
    let report = client.snapshot_workspace(&root, None).await?;
    let record = client.rebase_change(&report.change.to_string()).await?;
    client.apply_rebase(&root, &owner).await?;
    print_answer(json, &record, Some(&root));
    Ok(true)
}

fn print_answer(json: bool, record: &ChangeRecord, workspace: Option<&Path>) {
    if json {
        let rebase = json!({ "change": record, "workspace": workspace });
        println!("{}", json!({ "schema": REBASE_SCHEMA, "rebase": rebase }));
        return;
    }
    let change = &record.change;
    match change.parent {
        ChangeParent::Base(commit) => println!("rebased change {} onto {commit}", change.id),
        ChangeParent::Change(below) => println!("change {} stays on change {below}", change.id),
    }
    if let Some(root) = workspace {
        println!("wrote its head {} into {}", change.head, root.display());
    }
    if !change.conflicts.is_empty() {
        println!(
            "{} open conflict(s): combine both sides between the conflict markers, \
             then run `rosen resolve <file>`",
            change.conflicts.len()
        );
    }
}
