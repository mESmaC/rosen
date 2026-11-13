//! A background write-through watcher a host can start and stop per
//! workspace (SPEC.md R2). Velocity runs one per agent workspace, so the
//! watcher is a handle rather than a loop the caller drives.

use crate::snapshot::{self, WatchEvent};
use crate::Error;
use rosen_protocol::Endpoint;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Reports kept for a host that is not reading them. Reports are only
/// informational, since the watcher retries failures itself, so once this
/// many are unread the newest are dropped rather than growing without bound.
const REPORT_BUFFER: usize = 256;

/// A running watcher. Dropping the handle stops it, as [`Self::stop`] does
/// without waiting.
pub struct WorkspaceWatch {
    root: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), Error>>>,
    reports: mpsc::Receiver<WatchEvent>,
}

impl WorkspaceWatch {
    pub(super) fn start(endpoint: Endpoint, root: PathBuf) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let (sender, reports) = mpsc::channel(REPORT_BUFFER);
        let watched = root.clone();
        let task = tokio::spawn(async move {
            // A dropped sender also resolves `stopped`, so dropping the
            // handle ends the watch.
            let stop_signal = async {
                let _ = stopped.await;
            };
            snapshot::watch(&endpoint, &watched, stop_signal, move |event| {
                let _ = sender.try_send(event);
            })
            .await
        });
        Self {
            root,
            stop: Some(stop),
            task: Some(task),
            reports,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The next snapshot or failed attempt. `None` once the watcher has
    /// ended; [`Self::stop`] then gives the reason.
    pub async fn next_report(&mut self) -> Option<WatchEvent> {
        self.reports.recv().await
    }

    /// True once the watcher has ended on its own: its change closed, or the
    /// workspace could not be watched.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stops the watcher and waits for it to end. Writes it saw but had not
    /// snapshotted yet are not flushed; take a [`super::Session::snapshot`]
    /// afterwards when they must be stored. Returns the error that ended the
    /// watcher early, if one did.
    pub async fn stop(mut self) -> Result<(), Error> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await
            .map_err(|err| Error::Unexpected(format!("workspace watcher stopped: {err}")))?
    }
}

impl Drop for WorkspaceWatch {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
