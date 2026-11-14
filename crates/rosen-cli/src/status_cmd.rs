//! `rosen status` (SPEC.md R10): one change as the server holds it, and for a
//! local workspace, whatever it holds that the server does not have yet.

use crate::commands::StatusArgs;
use crate::server::Succeeded;
use rosen_client::protocol::{ChangeRecord, ClaimFilter, PathClaim};
use rosen_client::snapshot::{pending_journal, SnapshotError, WorkspaceMarker};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::{ChangeParent, ObjectId};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::output::STATUS_SCHEMA;

/// What a local workspace holds beyond the server's copy of its change.
#[derive(Serialize)]
struct WorkspaceState {
    path: PathBuf,
    /// The tree this workspace last stored or was written from.
    stored_tree: ObjectId,
    /// Snapshots captured offline and not yet acknowledged by the server.
    journal_pending: usize,
    /// Files whose content differs from what the server holds: those the
    /// waiting journal records carry, and those edited on disk since the
    /// newest captured snapshot.
    unsnapshotted: Vec<String>,
    /// The server moved the change head past this workspace's tree by a
    /// rebase, which is waiting to be applied (SPEC.md R2). A newer snapshot
    /// stored from another workspace of the change does not count.
    rebase_waiting: bool,
}

pub async fn run(json: bool, endpoint: &Endpoint, args: &StatusArgs) -> Result<Succeeded, Error> {
    let workspace = workspace_root(args)?;
    let change = match (&args.change, &workspace) {
        (Some(change), _) => change.clone(),
        (None, Some(root)) => WorkspaceMarker::read(root)?.change.to_string(),
        (None, None) => crate::claim_cmd::change_of(None).map_err(|_| {
            Error::InvalidArgument(
                "no change: run inside a workspace, pass --change or set ROSEN_CHANGE".into(),
            )
        })?,
    };
    let client = rosen_client::connect_or_start(endpoint).await?;
    let record = client.change_show(&change).await?;
    let filter = ClaimFilter {
        change: Some(record.change.id.to_string()),
    };
    let claims = client.claim_list(filter).await?;
    let local = match workspace {
        Some(root) => Some(scan_workspace(&client, &root, &record).await?),
        None => None,
    };
    if json {
        let body = json!({
            "schema": STATUS_SCHEMA,
            "change": record,
            "claims": claims,
            "workspace": local,
        });
        println!("{body}");
    } else {
        print_text(&record, &claims, local.as_ref());
    }
    Ok(true)
}

/// The workspace to scan: `--workspace`, else the one holding the current
/// directory unless `--change` names a change without one.
fn workspace_root(args: &StatusArgs) -> Result<Option<PathBuf>, Error> {
    if let Some(dir) = &args.workspace {
        return WorkspaceMarker::find_root(dir)
            .map(Some)
            .ok_or_else(|| Error::Snapshot(SnapshotError::NoWorkspace(dir.clone())));
    }
    if args.change.is_some() {
        return Ok(None);
    }
    Ok(WorkspaceMarker::find_root(&std::env::current_dir()?))
}

async fn scan_workspace(
    client: &Client,
    root: &Path,
    record: &ChangeRecord,
) -> Result<WorkspaceState, Error> {
    let marker = WorkspaceMarker::read(root)?;
    if marker.change != record.change.id {
        return Err(Error::InvalidArgument(format!(
            "{} is the workspace of change {}, not {}",
            root.display(),
            marker.change,
            record.change.id
        )));
    }
    let mut snapshotter = client.open_snapshotter(root).await?;
    let pending = pending_journal(root)?;
    // Edits not captured yet, plus, while journal records wait, the files
    // only those records carry: together everything the server lacks (#98).
    let unsnapshotted = tokio::task::spawn_blocking(move || {
        let mut paths = snapshotter.uncaptured_paths()?;
        if pending > 0 {
            paths.extend(snapshotter.unuploaded_paths()?);
            paths.sort();
            paths.dedup();
        }
        Ok::<_, SnapshotError>(paths)
    })
    .await
    .map_err(|err| Error::Unexpected(format!("workspace scan stopped: {err}")))??;
    Ok(WorkspaceState {
        path: root.to_path_buf(),
        stored_tree: marker.tree,
        journal_pending: pending,
        rebase_waiting: pending == 0 && rebased_past(record, marker.tree),
        unsnapshotted,
    })
}

/// True when the change head is neither this workspace's tree nor simply
/// the change's newest snapshot, so only a server rebase can have set it.
/// A rebase moves the head without a snapshot, and a snapshot taken after
/// one keeps its own tree while the head holds the merge onto the rebase.
fn rebased_past(record: &ChangeRecord, local: ObjectId) -> bool {
    let change = &record.change;
    change.head != local && change.snapshots.last().map(|s| s.tree) != Some(change.head)
}

fn print_text(record: &ChangeRecord, claims: &[PathClaim], local: Option<&WorkspaceState>) {
    let change = &record.change;
    println!("change:    {}", change.id);
    println!("owner:     {}", change.owner);
    println!("state:     {}", change.state);
    match &change.parent {
        ChangeParent::Base(commit) => println!("parent:    base {commit}"),
        ChangeParent::Change(parent) => println!("parent:    change {parent}"),
    }
    println!("head:      {}", change.head);
    println!("snapshots: {}", change.snapshots.len());
    println!("conflicts: {}", change.conflicts.len());
    for conflict in &change.conflicts {
        println!("  {conflict}");
    }
    println!("claims:    {}", claims.len());
    for claim in claims {
        let kind = format!("{:?}", claim.kind).to_lowercase();
        println!("  {kind} {}", claim.path);
    }
    let Some(local) = local else { return };
    println!("workspace: {}", local.path.display());
    if local.journal_pending > 0 {
        let count = local.journal_pending;
        println!("journal:   {count} snapshot(s) waiting for the server");
    }
    if local.rebase_waiting {
        println!("rebase:    the server rebased this change; apply it to the workspace");
    }
    if local.unsnapshotted.is_empty() {
        println!("files:     every file is stored on the server");
        return;
    }
    println!("files not yet stored on the server:");
    for path in &local.unsnapshotted {
        println!("  {path}");
    }
}
