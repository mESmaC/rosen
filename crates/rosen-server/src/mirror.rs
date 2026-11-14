//! The git mirror (SPEC R1, D12): after each land, the server pushes the
//! base to the configured git remote.
//!
//! One background task per server does every push, off the land path: a
//! land only publishes its `landed` event, which wakes the task. The task
//! pushes whatever the base holds by then, so lands that arrive during a
//! push are carried by the next one instead of each starting their own.
//! A failed push publishes a `mirror` event and is retried with backoff
//! until it succeeds; nothing about it blocks or undoes a land.
//!
//! The remote is the one the base's project configuration names, if it
//! names one, and otherwise the one `rosen import --mirror` stored.
//!
//! The task also syncs once at startup, so a land whose push a crash
//! interrupted reaches the mirror when the server comes back, and after an
//! import, which sets the base without a land.

mod push;
mod state;

use std::sync::Arc;
use std::time::Duration;

use rosen_core::ObjectId;
use rosen_protocol::EventKind;
use rosen_store::LooseObjectStore;
use serde_json::json;
use tokio::sync::watch;

pub use push::{hide_credentials, PushFailure};
pub use state::{MirrorStore, MirrorTarget};

use crate::landing::Ledger;
use crate::project_config;
use crate::{ServerConfig, ServerState};

/// The first retry waits this long; each further failure doubles the wait.
const RETRY_START: Duration = Duration::from_secs(1);
/// The longest wait between retries, so a mirror that comes back is caught
/// up within minutes without retrying a dead remote many times a second.
const RETRY_CAP: Duration = Duration::from_secs(5 * 60);

/// What one sync found and did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Synced {
    /// The project has no base or no mirror remote.
    NotConfigured,
    /// The remote already had the base commit.
    UpToDate { branch: String, commit: ObjectId },
    /// The remote accepted the base commit.
    Pushed { branch: String, commit: ObjectId },
}

/// Why a sync failed. Every failure is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    /// Reading or writing the metadata store failed.
    Store(String),
    Push {
        remote: String,
        branch: String,
        commit: ObjectId,
        failure: PushFailure,
    },
}

/// Pushes the base to the mirror until shutdown is requested.
pub async fn run(state: Arc<ServerState>) {
    let mut wake = state.events.watch();
    let mut failures: u32 = 0;
    loop {
        // Lands after this point wake the next sync, even if they arrive
        // while this one is pushing.
        let seen = state.events.last_seq();
        let keep_going = match sync_once(&state.config).await {
            Ok(synced) => {
                if failures > 0 {
                    report_recovered(&state, &synced, failures);
                }
                failures = 0;
                wait_for_land(&state, &mut wake, seen).await
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                let delay = retry_delay(failures);
                report_failure(&state, &error, failures, delay);
                sleep_unless_shutdown(&state, delay).await
            }
        };
        if !keep_going {
            return;
        }
    }
}

/// Brings the mirror up to the current base once.
pub async fn sync_once(config: &ServerConfig) -> Result<Synced, SyncError> {
    let reading = config.clone();
    let found = tokio::task::spawn_blocking(move || read_target(&reading))
        .await
        .map_err(|join| SyncError::Store(join.to_string()))??;
    let Some((target, branch, commit)) = found else {
        return Ok(Synced::NotConfigured);
    };
    if target.pushed == Some(commit) {
        return Ok(Synced::UpToDate { branch, commit });
    }
    let staging = config.mirror_staging_dir();
    let objects = config.objects_dir();
    push::push(&staging, &objects, &target.remote, &branch, &commit)
        .await
        .map_err(|failure| SyncError::Push {
            remote: hide_credentials(&target.remote, &target.remote),
            branch: branch.clone(),
            commit,
            failure,
        })?;
    let metadata = config.metadata_path();
    tokio::task::spawn_blocking(move || {
        MirrorStore::open(&metadata)?.record_pushed(&target.remote, &commit)
    })
    .await
    .map_err(|join| SyncError::Store(join.to_string()))?
    .map_err(|error| SyncError::Store(error.to_string()))?;
    Ok(Synced::Pushed { branch, commit })
}

/// The mirror target with the base's name and commit, or `None` when there
/// is nothing to push yet.
fn read_target(
    config: &ServerConfig,
) -> Result<Option<(MirrorTarget, String, ObjectId)>, SyncError> {
    let metadata = config.metadata_path();
    // A server that has never imported has no database; opening one here
    // would create it.
    if !metadata.is_file() {
        return Ok(None);
    }
    let store = |error: String| SyncError::Store(error);
    let base = Ledger::open(&metadata)
        .and_then(|ledger| ledger.project_base())
        .map_err(|e| store(e.to_string()))?;
    let Some((branch, commit)) = base else {
        return Ok(None);
    };
    let mirror = MirrorStore::open(&metadata).map_err(|e| store(e.to_string()))?;
    adopt_configured_remote(config, &mirror, &commit)?;
    let target = mirror.target().map_err(|e| store(e.to_string()))?;
    Ok(target.map(|target| (target, branch, commit)))
}

/// A remote named by the base's project configuration replaces the stored
/// one (SPEC R1), so a land that changes `mirror.remote` takes effect with
/// its own push. Storing it keeps the pushed commit tracked per remote.
fn adopt_configured_remote(
    config: &ServerConfig,
    mirror: &MirrorStore,
    base: &ObjectId,
) -> Result<(), SyncError> {
    let store = |error: String| SyncError::Store(error);
    let objects = LooseObjectStore::open(config.objects_dir()).map_err(|e| store(e.to_string()))?;
    let in_force = project_config::resolve(&objects, base).map_err(|e| store(e.to_string()))?;
    let Some(remote) = &in_force.config.mirror.remote else {
        return Ok(());
    };
    let stored = mirror.target().map_err(|e| store(e.to_string()))?;
    if stored.is_some_and(|target| target.remote == *remote) {
        return Ok(());
    }
    mirror.set_remote(remote).map_err(|e| store(e.to_string()))
}

/// Waits until a land is published after `seen`, or an import wakes the
/// mirror. Returns false when the server is shutting down instead.
async fn wait_for_land(state: &ServerState, wake: &mut watch::Receiver<u64>, seen: u64) -> bool {
    let mut seen = seen;
    loop {
        let fresh = state.events.after(seen);
        if fresh.iter().any(|event| event.kind == EventKind::Landed) {
            return true;
        }
        if let Some(last) = fresh.last() {
            seen = last.seq;
        }
        tokio::select! {
            changed = wake.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            () = state.mirror_wake.notified() => return true,
            () = state.shutdown_requested() => return false,
        }
    }
}

async fn sleep_unless_shutdown(state: &ServerState, delay: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = state.shutdown_requested() => false,
    }
}

fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    RETRY_START.saturating_mul(1 << doublings).min(RETRY_CAP)
}

/// Publishes a `mirror` event for a failed push (SPEC R1, R9).
fn report_failure(state: &ServerState, error: &SyncError, attempt: u32, retry_in: Duration) {
    let retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX);
    let detail = match error {
        SyncError::Store(message) => json!({
            "status": "failed",
            "attempt": attempt,
            "retry_in_ms": retry_in_ms,
            "rejected": false,
            "error": message,
        }),
        SyncError::Push {
            remote,
            branch,
            commit,
            failure,
        } => json!({
            "status": "failed",
            "remote": remote,
            "branch": branch,
            "commit": commit.to_hex(),
            "attempt": attempt,
            "retry_in_ms": retry_in_ms,
            "rejected": failure.rejected,
            "error": failure.message,
        }),
    };
    state.events.publish(EventKind::Mirror, None, detail);
}

/// Publishes a `mirror` event when the mirror is current again after
/// failures, so whoever saw the failure knows it is over.
fn report_recovered(state: &ServerState, synced: &Synced, failures: u32) {
    let (Synced::Pushed { branch, commit } | Synced::UpToDate { branch, commit }) = synced else {
        return;
    };
    let detail = json!({
        "status": "pushed",
        "branch": branch,
        "commit": commit.to_hex(),
        "after_failures": failures,
    });
    state.events.publish(EventKind::Mirror, None, detail);
}
