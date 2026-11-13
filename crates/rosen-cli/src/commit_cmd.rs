//! `rosen commit` and `rosen workspace watch` (SPEC R2): named and automatic
//! snapshots of a local workspace.

use crate::server::Succeeded;
use rosen_client::snapshot::{SnapshotError, SnapshotReport, WatchEvent, WorkspaceMarker};
use rosen_client::{Endpoint, Error};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Schema tag for `--json` output (SPEC R10). Bump on any shape change.
const SNAPSHOT_SCHEMA: &str = "rosen.snapshot/v1";

/// Stores a named snapshot of the workspace holding `workspace`, or the
/// current directory.
pub async fn commit(
    json: bool,
    endpoint: &Endpoint,
    message: &str,
    workspace: Option<&Path>,
) -> Result<Succeeded, Error> {
    let root = workspace_root(workspace)?;
    let client = rosen_client::connect_or_start(endpoint).await?;
    let report = client
        .snapshot_workspace(&root, Some(message.to_owned()))
        .await?;
    print_report(json, &report);
    Ok(true)
}

/// Snapshots the workspace on every write until Ctrl-C.
pub async fn watch(
    json: bool,
    endpoint: &Endpoint,
    workspace: Option<&Path>,
) -> Result<Succeeded, Error> {
    let root = workspace_root(workspace)?;
    // Start the daemon if needed; the watcher makes its own connections.
    drop(rosen_client::connect_or_start(endpoint).await?);
    if !json {
        eprintln!("watching {} (Ctrl-C stops)", root.display());
    }
    let stop = async {
        // A failed handler install leaves no way to stop but closing the
        // terminal, which also ends the process.
        let _ = tokio::signal::ctrl_c().await;
    };
    rosen_client::snapshot::watch(endpoint, &root, stop, |event| match event {
        WatchEvent::Snapshot(report) if report.recorded => print_report(json, &report),
        WatchEvent::Snapshot(_) => {}
        WatchEvent::Failed(err) if json => {
            let body = json!({"schema": SNAPSHOT_SCHEMA, "error": err.to_string()});
            println!("{body}");
        }
        WatchEvent::Failed(err) => eprintln!("rosen: snapshot failed, retrying: {err}"),
    })
    .await?;
    Ok(true)
}

fn workspace_root(given: Option<&Path>) -> Result<PathBuf, Error> {
    let start = match given {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    WorkspaceMarker::find_root(&start).ok_or(Error::Snapshot(SnapshotError::NoWorkspace(start)))
}

fn print_report(json: bool, report: &SnapshotReport) {
    if json {
        let body = json!({
            "schema": SNAPSHOT_SCHEMA,
            "change": report.change,
            "tree": report.tree,
            "commit": report.commit,
            "recorded": report.recorded,
            "changed_files": report.changed_files,
            "removed_files": report.removed,
            "blobs_sent": report.sent.blobs,
            "trees_sent": report.sent.trees,
            "bytes_sent": report.sent.bytes,
        });
        println!("{body}");
        return;
    }
    let commit = report
        .commit
        .map_or_else(|| "-".to_owned(), |commit| commit.to_string());
    println!(
        "snapshot {commit} of change {}: {} changed, {} removed, {} bytes sent",
        report.change,
        report.changed_files.len(),
        report.removed,
        report.sent.bytes
    );
}
