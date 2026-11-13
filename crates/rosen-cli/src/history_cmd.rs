//! `rosen log` and `rosen diff` (SPEC R10, R11): history read from the
//! server, with no checkout.

use crate::server::Succeeded;
use rosen_client::protocol::{Diff, DiffRequest, FileDiff, LineKind, Log, LogRequest};
use rosen_client::snapshot::WorkspaceMarker;
use rosen_client::{Endpoint, Error};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Schema tags for `--json` output (SPEC R10). Bump on any shape change.
const LOG_SCHEMA: &str = "rosen.log/v1";
const DIFF_SCHEMA: &str = "rosen.diff/v1";

pub async fn log(
    json: bool,
    endpoint: &Endpoint,
    change: Option<&str>,
    paths: &[PathBuf],
    limit: Option<usize>,
) -> Result<Succeeded, Error> {
    let request = LogRequest {
        at: change.map(str::to_owned),
        paths: repository_paths(paths)?,
        limit: limit.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
    };
    let client = rosen_client::connect_or_start(endpoint).await?;
    print_log(json, &client.log(request).await?);
    Ok(true)
}

pub async fn diff(
    json: bool,
    endpoint: &Endpoint,
    change: Option<&str>,
    paths: &[PathBuf],
) -> Result<Succeeded, Error> {
    let request = DiffRequest {
        to: Some(change_of(change)?),
        paths: repository_paths(paths)?,
        ..DiffRequest::default()
    };
    let client = rosen_client::connect_or_start(endpoint).await?;
    print_diff(json, &client.diff(request).await?);
    Ok(true)
}

/// `--change`, then the workspace holding the current directory, then
/// $ROSEN_CHANGE, which Velocity sets for each agent thread.
fn change_of(explicit: Option<&str>) -> Result<String, Error> {
    if let Some(change) = explicit {
        return Ok(change.to_owned());
    }
    let cwd = std::env::current_dir()?;
    if let Some(root) = WorkspaceMarker::find_root(&cwd) {
        return Ok(WorkspaceMarker::read(&root)?.change.to_string());
    }
    std::env::var("ROSEN_CHANGE")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::InvalidArgument(
                "no change: pass --change, run inside a workspace, or set ROSEN_CHANGE".into(),
            )
        })
}

/// Paths as the repository names them. Inside a workspace, a relative path
/// is taken from the current directory, as a shell user expects; elsewhere
/// it is already repository-relative.
fn repository_paths(paths: &[PathBuf]) -> Result<Vec<String>, Error> {
    let cwd = std::env::current_dir()?;
    let root = WorkspaceMarker::find_root(&cwd);
    paths
        .iter()
        .map(|path| {
            let relative = match &root {
                Some(root) => within_workspace(root, &cwd.join(path))?,
                None => path.clone(),
            };
            Ok(relative.to_string_lossy().replace('\\', "/"))
        })
        .collect()
}

fn within_workspace(root: &Path, path: &Path) -> Result<PathBuf, Error> {
    // Strip `.` and `..` lexically: the path may not exist at this revision.
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other),
        }
    }
    clean
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| Error::InvalidArgument(format!("{} is outside the workspace", path.display())))
}

fn print_log(json: bool, log: &Log) {
    if json {
        println!("{}", json!({ "schema": LOG_SCHEMA, "log": log }));
        return;
    }
    for entry in &log.entries {
        let change = entry.change.map(|c| format!("  [{c}]")).unwrap_or_default();
        println!(
            "{}  {}  {}{change}",
            entry.commit,
            utc_date(entry.time),
            entry.summary
        );
    }
    if log.truncated {
        println!("(more history: raise --limit)");
    }
}

fn print_diff(json: bool, diff: &Diff) {
    if json {
        println!("{}", json!({ "schema": DIFF_SCHEMA, "diff": diff }));
        return;
    }
    for file in &diff.files {
        print_file(file);
    }
}

fn print_file(file: &FileDiff) {
    let side = |present: bool, prefix: &str| {
        if present {
            format!("{prefix}/{}", file.path)
        } else {
            "/dev/null".to_owned()
        }
    };
    println!("diff --rosen a/{0} b/{0}", file.path);
    if let (Some(old), Some(new)) = (&file.old, &file.new) {
        if old.mode != new.mode {
            println!("old mode {}\nnew mode {}", old.mode, new.mode);
        }
    }
    if file.binary {
        println!("Binary files differ");
        return;
    }
    println!("--- {}", side(file.old.is_some(), "a"));
    println!("+++ {}", side(file.new.is_some(), "b"));
    for hunk in &file.hunks {
        println!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        );
        for line in &hunk.lines {
            let mark = match line.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            };
            println!("{mark}{}", line.text);
        }
    }
}

/// `YYYY-MM-DD` in UTC, from Unix seconds (the civil-from-days algorithm).
fn utc_date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
