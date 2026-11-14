//! The workspace watcher (SPEC R2): file system events in, snapshots out.
//!
//! Events come from `notify`, which uses `ReadDirectoryChangesW` on Windows.
//! A snapshot starts once writes pause for [`QUIET`], or [`MAX_WAIT`] after
//! the first unsnapshotted write while writes keep coming, so a write is on
//! the server well inside SPEC R2's 2 s. Writes to ignored paths are dropped
//! before they reach the timer, so they never delay a snapshot.

use super::{journal_snapshot, take_snapshot, Dirty, SnapshotReport, Snapshotter};
use crate::{Client, Endpoint, Error};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rosen_protocol::ErrorCode;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant};

/// How long writes must pause before a snapshot starts.
pub const QUIET: Duration = Duration::from_millis(250);
/// The longest a write waits for a snapshot while writes keep coming.
pub const MAX_WAIT: Duration = Duration::from_millis(1000);
/// How long a failed snapshot waits before it is tried again.
pub const RETRY: Duration = Duration::from_secs(1);

/// What the watcher tells its caller after each attempt.
#[derive(Debug)]
pub enum WatchEvent {
    Snapshot(SnapshotReport),
    /// The attempt failed; its paths are kept and retried after [`RETRY`].
    Failed(Error),
}

/// Watches the workspace at `root` and snapshots it until `stop` resolves.
/// The first snapshot covers the whole workspace, catching writes made while
/// no watcher ran. `report` hears about every attempt. When the server goes
/// away, the watcher keeps the unsnapshotted paths and connects again on
/// each retry.
pub async fn watch(
    endpoint: &Endpoint,
    root: &Path,
    stop: impl Future<Output = ()>,
    mut report: impl FnMut(WatchEvent),
) -> Result<(), Error> {
    let client = Client::connect(endpoint).await.ok();
    let mut snapshotter = match &client {
        Some(client) => client.open_snapshotter(root).await?,
        None => Snapshotter::open_offline(root)?,
    };
    snapshotter.defer_unreadable();
    let mut link = Link {
        endpoint: endpoint.clone(),
        client,
    };
    let (sender, mut events) = mpsc::unbounded_channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        // The receiver is gone only once the watch has stopped.
        let _ = sender.send(event);
    })
    .map_err(watch_error)?;
    watcher
        .watch(snapshotter.root(), RecursiveMode::Recursive)
        .map_err(watch_error)?;
    tokio::pin!(stop);
    let mut pending = Pending::new(Dirty::Full, Instant::now());
    loop {
        let wake = pending.as_ref().map(Pending::due);
        tokio::select! {
            () = &mut stop => return Ok(()),
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                note(&mut snapshotter, &mut pending, event);
            }
            () = sleep_or_never(wake) => {
                let dirty = pending.take().expect("a wake time means a pending snapshot").dirty;
                let (back, result) = link.snapshot(snapshotter, dirty.clone()).await;
                snapshotter = back;
                pending = after_attempt(result, dirty, &mut report)?;
            }
        }
    }
}

/// Reports one attempt and returns what is left to snapshot.
fn after_attempt(
    result: Result<SnapshotReport, Error>,
    dirty: Dirty,
    report: &mut impl FnMut(WatchEvent),
) -> Result<Option<Pending>, Error> {
    match result {
        Ok(done) => {
            // A held file is read again once it may be free; the writes
            // stored meanwhile did not wait for it.
            let retry = (!done.unreadable.is_empty())
                .then(|| Pending::retry(Dirty::Paths(done.unreadable.iter().cloned().collect())))
                .flatten();
            report(WatchEvent::Snapshot(done));
            Ok(retry)
        }
        // The change is closed or gone: no retry can store it.
        Err(err) if is_final(&err) => Err(err),
        Err(err) => {
            report(WatchEvent::Failed(err));
            Ok(Pending::retry(dirty))
        }
    }
}

/// The watcher's connection, made again after the server goes away.
struct Link {
    endpoint: Endpoint,
    client: Option<Client>,
}

impl Link {
    async fn snapshot(
        &mut self,
        snapshotter: Snapshotter,
        dirty: Dirty,
    ) -> (Snapshotter, Result<SnapshotReport, Error>) {
        let client = match self.client.take() {
            Some(client) => client,
            None => match Client::connect(&self.endpoint).await {
                Ok(client) => client,
                Err(_) => return journal_snapshot(snapshotter, dirty).await,
            },
        };
        let (back, result) = take_snapshot(&client, snapshotter, dirty, None).await;
        // A connection that failed is dropped; the retry makes a new one.
        let lost = matches!(
            result,
            Err(Error::Disconnected | Error::NotRunning(_) | Error::Io(_))
        );
        if !lost {
            self.client = Some(client);
        }
        (back, result)
    }
}

/// Paths waiting for a snapshot and when it is due.
struct Pending {
    dirty: Dirty,
    first: Instant,
    last: Instant,
    /// Set after a failure: no snapshot before this time.
    not_before: Option<Instant>,
}

impl Pending {
    fn new(dirty: Dirty, now: Instant) -> Option<Self> {
        Some(Self {
            dirty,
            first: now,
            last: now,
            not_before: None,
        })
    }

    /// A failed snapshot's paths, tried again after [`RETRY`]. Events that
    /// arrived meanwhile are still queued and join it as they are read.
    fn retry(dirty: Dirty) -> Option<Self> {
        let now = Instant::now();
        Some(Self {
            dirty,
            first: now,
            last: now,
            not_before: Some(now + RETRY),
        })
    }

    fn due(&self) -> Instant {
        let due = (self.last + QUIET).min(self.first + MAX_WAIT);
        self.not_before.map_or(due, |floor| due.max(floor))
    }

    fn add(&mut self, more: &Dirty, at: Instant) {
        self.last = at;
        match (&mut self.dirty, more) {
            (Dirty::Full, _) => {}
            (dirty, Dirty::Full) => *dirty = Dirty::Full,
            (Dirty::Paths(paths), Dirty::Paths(new)) => paths.extend(new.iter().cloned()),
        }
    }
}

/// Adds an event's paths to the pending snapshot, skipping ignored ones.
fn note(
    snapshotter: &mut Snapshotter,
    pending: &mut Option<Pending>,
    event: notify::Result<notify::Event>,
) {
    let dirty = match event {
        // The OS dropped events (its buffer overflowed): rescan everything.
        Err(_) => Dirty::Full,
        Ok(event) if event.need_rescan() => Dirty::Full,
        Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => return,
        Ok(event) => match relevant_paths(snapshotter, &event.paths) {
            Some(paths) if paths.is_empty() => return,
            Some(paths) => Dirty::Paths(paths),
            None => Dirty::Full,
        },
    };
    let now = Instant::now();
    match pending {
        Some(waiting) => waiting.add(&dirty, now),
        None => *pending = Pending::new(dirty, now),
    }
}

/// The event's paths relative to the root, without ignored ones. `None` when
/// a path is outside the root, which only a renamed root can cause.
fn relevant_paths(
    snapshotter: &mut Snapshotter,
    paths: &[std::path::PathBuf],
) -> Option<BTreeSet<String>> {
    let mut relevant = BTreeSet::new();
    for path in paths {
        let rel = path.strip_prefix(snapshotter.root()).ok()?;
        let Some(rel) = rel.to_str() else { continue };
        let rel = rel.replace('\\', "/");
        if rel.is_empty() {
            return None;
        }
        if snapshotter.is_relevant(&rel) {
            relevant.insert(rel);
        }
    }
    Some(relevant)
}

fn is_final(err: &Error) -> bool {
    matches!(err, Error::Server(body) if matches!(body.code, ErrorCode::Refused | ErrorCode::NotFound))
}

async fn sleep_or_never(wake: Option<Instant>) {
    match wake {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn watch_error(err: notify::Error) -> Error {
    Error::Io(std::io::Error::other(format!("file watcher: {err}")))
}
