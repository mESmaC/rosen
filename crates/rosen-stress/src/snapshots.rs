//! Automatic snapshot latency under the same load as the stress agents.

use anyhow::{anyhow, Context, Result};
use rosen_client::protocol::{ErrorCode, ReadAnswer, ReadRequest};
use rosen_client::{Client, Endpoint, Error, ObjectId, Session, WatchEvent, WorkspaceWatch};
use std::path::Path;
use std::time::{Duration, Instant};

// A hang guard, independent of R2's measured two-second budget.
const WATCH_TIMEOUT: Duration = Duration::from_secs(180);

/// Wait for the initial full scan before measuring writes to a running watcher.
pub async fn start(session: &Session, root: &Path) -> Result<WorkspaceWatch> {
    let mut watch = session.watch(root);
    tokio::time::timeout(WATCH_TIMEOUT, next_snapshot(&mut watch))
        .await
        .context("watcher startup timed out")??;
    Ok(watch)
}

/// Time the first acknowledged snapshot that contains every edit. Validate
/// its immutable commit, so a partial or stale snapshot cannot be a sample.
pub async fn measure(
    session: &Session,
    watch: &mut WorkspaceWatch,
    expected: &[(String, String)],
    written: Instant,
) -> Result<f64> {
    tokio::time::timeout(WATCH_TIMEOUT, async {
        loop {
            let report = next_snapshot(watch).await?;
            let ms = written.elapsed().as_secs_f64() * 1000.0;
            let Some(commit) = report.commit else {
                continue;
            };
            if contains_edits(session.endpoint(), commit, expected).await? {
                return Ok(ms);
            }
        }
    })
    .await
    .context("automatic snapshot timed out")?
}

async fn contains_edits(
    endpoint: &Endpoint,
    commit: ObjectId,
    expected: &[(String, String)],
) -> Result<bool> {
    loop {
        // The watch may have reconnected after the agent's session was lost.
        let checked = async {
            let client = Client::connect(endpoint).await?;
            for (path, line) in expected {
                let request = ReadRequest {
                    at: Some(commit.to_string()),
                    path: path.clone(),
                };
                match client.read_file(request).await {
                    Ok(ReadAnswer::File { data, .. })
                        if String::from_utf8_lossy(&data)
                            .lines()
                            .any(|found| found == line) => {}
                    Ok(_) => return Ok(false),
                    Err(Error::Server(body)) if body.code == ErrorCode::NotFound => {
                        return Ok(false)
                    }
                    Err(err) => return Err(err),
                }
            }
            Ok(true)
        }
        .await;
        match checked {
            Ok(complete) => return Ok(complete),
            Err(err) if crate::agent::is_transient(&err) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

async fn next_snapshot(
    watch: &mut WorkspaceWatch,
) -> Result<rosen_client::snapshot::SnapshotReport> {
    loop {
        match watch.next_report().await {
            Some(WatchEvent::Snapshot(report)) => return Ok(report),
            // The watcher retries transport failures across the stress run's faults.
            Some(WatchEvent::Failed(_)) => {}
            None => return Err(anyhow!("watcher ended before a snapshot")),
        }
    }
}
