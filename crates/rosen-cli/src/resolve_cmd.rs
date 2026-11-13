//! `rosen resolve` (SPEC R5, R10): clears a change's stored conflicts in some
//! files so it can land.

use std::path::{Component, Path, PathBuf};

use crate::output::RESOLVE_SCHEMA;
use crate::server::Succeeded;
use rosen_client::protocol::ConflictsResolved;
use rosen_client::snapshot::WorkspaceMarker;
use rosen_client::{Client, Endpoint, Error};
use serde_json::json;

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    change: Option<&str>,
    paths: &[PathBuf],
) -> Result<Succeeded, Error> {
    let cwd = std::env::current_dir()?;
    let workspace = WorkspaceMarker::find_root(&cwd).filter(|_| change.is_none());
    let (client, change, paths) = match workspace {
        // Checked before connecting, so a bad command line never starts a server.
        None => {
            let (change, paths) = (crate::claim_cmd::change_of(change)?, from_root(paths)?);
            (
                rosen_client::connect_or_start(endpoint).await?,
                change,
                paths,
            )
        }
        Some(root) => {
            let client = rosen_client::connect_or_start(endpoint).await?;
            let (change, paths) = in_workspace(&client, &root, &cwd, paths).await?;
            (client, change, paths)
        }
    };
    let answer = client.resolve_conflicts(&change, paths).await?;
    print_answer(json, &answer);
    Ok(true)
}

/// Snapshots the workspace before resolving: the land that follows merges
/// the change's stored head, so the fixed files must be on the server.
async fn in_workspace(
    client: &Client,
    root: &Path,
    cwd: &Path,
    paths: &[PathBuf],
) -> Result<(String, Vec<String>), Error> {
    let report = client.snapshot_workspace(root, None).await?;
    let paths = paths
        .iter()
        .map(|path| repo_relative(root, cwd, path))
        .collect::<Result<_, _>>()?;
    Ok((report.change.to_string(), paths))
}

/// Resolves `path` against `cwd` without touching the disk, since resolving
/// a delete conflict can leave the file absent, then strips the workspace root.
fn repo_relative(root: &Path, cwd: &Path, path: &Path) -> Result<String, Error> {
    let mut full = PathBuf::new();
    for part in cwd.join(path).components() {
        match part {
            Component::ParentDir => {
                full.pop();
            }
            Component::CurDir => {}
            other => full.push(other),
        }
    }
    let outside = || {
        let shown = path.display();
        Error::InvalidArgument(format!(
            "{shown} is outside the workspace at {}",
            root.display()
        ))
    };
    let relative = full.strip_prefix(root).map_err(|_| outside())?;
    if relative.as_os_str().is_empty() {
        return Err(outside());
    }
    Ok(slashed(relative))
}

/// Paths given with a named change are repository-relative.
fn from_root(paths: &[PathBuf]) -> Result<Vec<String>, Error> {
    paths
        .iter()
        .map(|path| {
            let inside = path
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
            if !inside {
                let shown = path.display();
                let why = format!("{shown} is not relative to the repository root");
                return Err(Error::InvalidArgument(why));
            }
            Ok(slashed(path))
        })
        .collect()
}

fn slashed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn print_answer(json: bool, answer: &ConflictsResolved) {
    if json {
        println!("{}", json!({ "schema": RESOLVE_SCHEMA, "resolve": answer }));
        return;
    }
    for path in &answer.resolved {
        println!("resolved {} conflict(s) in {}", path.conflicts, path.path);
    }
    if answer.open_paths.is_empty() {
        println!("change {} has no open conflicts", answer.change);
    } else {
        let open = answer.open_paths.join(" ");
        println!(
            "change {} still has open conflicts in: {open}",
            answer.change
        );
    }
}
