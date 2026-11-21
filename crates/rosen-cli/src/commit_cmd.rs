//! `rosen commit` and `rosen workspace watch` (SPEC R2, R10): named and
//! automatic snapshots of a local workspace.

use crate::server::Succeeded;
use rosen_client::protocol::{DiffRequest, FileDiff, FileStatus, LandCheck, LineKind};
use rosen_client::snapshot::{SnapshotError, SnapshotReport, WatchEvent, WorkspaceMarker};
use rosen_client::{Client, Endpoint, Error};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::output::SNAPSHOT_SCHEMA;

/// Compact per-file summary of a change against its parent after `commit`.
#[derive(Serialize)]
struct FileSummary {
    path: String,
    status: FileStatus,
    binary: bool,
    lines_added: usize,
    lines_removed: usize,
}

/// Post-commit details returned in the same response as `rosen commit`.
struct CommitSummary {
    files: Option<Vec<FileSummary>>,
    land_check: Option<LandCheck>,
}

/// Stores a named snapshot of the workspace holding `workspace`, or the
/// current directory, then reports the post-commit file summary and land check.
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
    let summary = fetch_commit_summary(&client, &report.change.to_string()).await;
    // Offline snapshots replayed ahead of this one get a line each.
    for earlier in &report.replayed {
        print_report(json, earlier, None);
    }
    print_report(json, &report, Some(&summary));
    Ok(true)
}

async fn fetch_commit_summary(client: &Client, change: &str) -> CommitSummary {
    let request = DiffRequest {
        to: Some(change.to_owned()),
        context: Some(0),
        ..DiffRequest::default()
    };
    let files = client
        .diff(request)
        .await
        .ok()
        .map(|diff| diff.files.iter().map(summarize_file).collect());
    let land_check = client.land_check(change).await.ok();
    CommitSummary { files, land_check }
}

fn summarize_file(file: &FileDiff) -> FileSummary {
    let (mut added, mut removed) = (0, 0);
    for hunk in &file.hunks {
        for line in &hunk.lines {
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                LineKind::Context => {}
            }
        }
    }
    FileSummary {
        path: file.path.clone(),
        status: file.status,
        binary: file.binary,
        lines_added: added,
        lines_removed: removed,
    }
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
    // A held file is retried every second; say so only when the set changes.
    let mut unreadable = Vec::new();
    rosen_client::snapshot::watch(endpoint, &root, stop, |event| match event {
        WatchEvent::Snapshot(report) if report.recorded || report.unreadable != unreadable => {
            unreadable.clone_from(&report.unreadable);
            print_report(json, &report, None);
        }
        WatchEvent::Snapshot(_) => {}
        WatchEvent::Failed(err) if json => {
            crate::output::print_error(Some("workspace watch"), &err.to_string());
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

fn print_report(json: bool, report: &SnapshotReport, summary: Option<&CommitSummary>) {
    if json {
        let mut body = json!({
            "schema": SNAPSHOT_SCHEMA,
            "change": report.change,
            "tree": report.tree,
            "commit": report.commit,
            "recorded": report.recorded,
            "changed_files": report.changed_files,
            "removed_files": report.removed,
            "unreadable_files": report.unreadable,
            "blobs_sent": report.sent.blobs,
            "trees_sent": report.sent.trees,
            "bytes_sent": report.sent.bytes,
        });
        if let Some(summary) = summary {
            body["files"] = json!(summary.files);
            body["land_check"] = json!(summary.land_check);
        }
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
    if !report.unreadable.is_empty() {
        println!(
            "  held open or still being written, kept at last stored content: {}",
            report.unreadable.join(", ")
        );
    }
    if let Some(summary) = summary {
        print_summary_text(summary);
    }
}

fn print_summary_text(summary: &CommitSummary) {
    if let Some(files) = &summary.files {
        println!("files:     {} changed in this change", files.len());
        for file in files {
            let code = match file.status {
                FileStatus::Added => 'A',
                FileStatus::Deleted => 'D',
                FileStatus::Modified => 'M',
            };
            if file.binary {
                println!("  {code} {} (binary)", file.path);
            } else {
                println!(
                    "  {code} {} (+{} -{})",
                    file.path, file.lines_added, file.lines_removed
                );
            }
        }
    }
    if let Some(gates) = &summary.land_check {
        crate::status_cmd::print_gates(gates);
    }
}
