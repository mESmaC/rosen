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
/// How long a failed snapshot waits before it is tried again, and how
/// often a held file is read again.
pub const RETRY: Duration = Duration::from_secs(1);
/// The longest pause between retries while failures repeat, as while the
/// server is unreachable. A new write is still captured at once.
pub const MAX_RETRY: Duration = Duration::from_secs(16);

/// What the watcher tells its caller after each attempt.
#[derive(Debug)]
pub enum WatchEvent {
    Snapshot(SnapshotReport),
    /// The attempt failed; it is retried after [`RETRY`], doubling while
    /// failures repeat up to [`MAX_RETRY`].
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
        // Opening waits for another writer's upload; keep it off the runtime.
        None => {
            let root = root.to_path_buf();
            tokio::task::spawn_blocking(move || Snapshotter::open_offline(&root))
                .await
                .map_err(|err| Error::Unexpected(format!("workspace open stopped: {err}")))??
        }
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
    let mut backoff = Backoff::default();
    loop {
        let wake = pending.as_ref().map(Pending::due);
        tokio::select! {
            () = &mut stop => return Ok(()),
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                note(&mut snapshotter, &mut pending, event);
            }
            () = sleep_or_never(wake) => {
                let due = pending.take().expect("a wake time means a pending snapshot");
                let (back, result, attempt) = link.attempt(snapshotter, due, &backoff).await;
                snapshotter = back;
                pending = after_attempt(result, attempt, &mut backoff, &mut report)?;
            }
        }
    }
}

/// What one attempt left behind.
struct Attempt {
    /// The paths still to snapshot.
    left: Dirty,
    /// The attempt was captured but skipped files another process holds.
    held: bool,
    /// The attempt only wrote the journal, without trying the server.
    journal_only: bool,
}

/// Reports one attempt and returns what is left to snapshot.
fn after_attempt(
    result: Result<SnapshotReport, Error>,
    attempt: Attempt,
    backoff: &mut Backoff,
    report: &mut impl FnMut(WatchEvent),
) -> Result<Option<Pending>, Error> {
    let left = attempt.left;
    match result {
        Ok(mut done) => {
            *backoff = Backoff::default();
            for earlier in std::mem::take(&mut done.replayed) {
                report(WatchEvent::Snapshot(earlier));
            }
            report(WatchEvent::Snapshot(done));
            // A held file is read again once it may be free; the writes
            // stored meanwhile did not wait for it.
            Ok(match left {
                Dirty::Paths(paths) if paths.is_empty() => None,
                left => Pending::retry(left, RETRY),
            })
        }
        // The change is closed or gone: no retry can store it.
        Err(err) if is_final(&err) => Err(err),
        Err(err) => {
            // A journal-only re-read did not try the server, so it is no
            // new failure and does not push the server's retry back.
            if !attempt.journal_only {
                backoff.failed();
            }
            report(WatchEvent::Failed(err));
            if attempt.held {
                return Ok(Pending::reread(left, RETRY));
            }
            // A captured attempt leaves no paths, and its retry only
            // replays the journal.
            Ok(Pending::retry(left, backoff.remaining()))
        }
    }
}

/// Failed attempts in a row, and when the server may be tried again.
#[derive(Default)]
struct Backoff {
    failures: u32,
    next_try: Option<Instant>,
}

impl Backoff {
    fn failed(&mut self) {
        self.failures += 1;
        self.next_try = Some(Instant::now() + backoff(self.failures));
    }

    /// Whether the server's retry is not due yet.
    fn waiting(&self) -> bool {
        self.next_try.is_some_and(|at| Instant::now() < at)
    }

    fn remaining(&self) -> Duration {
        self.next_try.map_or(Duration::ZERO, |at| {
            at.saturating_duration_since(Instant::now())
        })
    }
}

/// The pause before retry number `failures`: [`RETRY`], doubled for each
/// earlier failure in a row, at most [`MAX_RETRY`].
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(8);
    (RETRY * 2u32.pow(doublings)).min(MAX_RETRY)
}

/// The watcher's connection, made again after the server goes away.
struct Link {
    endpoint: Endpoint,
    client: Option<Client>,
}

impl Link {
    /// Takes the due snapshot and says what it left. A held file is read
    /// again each second (R2), but the server is tried again only on the
    /// backoff: until then, the re-read goes to the journal alone (#263).
    async fn attempt(
        &mut self,
        snapshotter: Snapshotter,
        due: Pending,
        backoff: &Backoff,
    ) -> (Snapshotter, Result<SnapshotReport, Error>, Attempt) {
        let journal_only = due.reread_only && backoff.waiting();
        let (snapshotter, result) = self
            .snapshot(snapshotter, due.dirty.clone(), journal_only)
            .await;
        // A captured snapshot is on the server or in the journal, so only
        // the files it skipped need another read.
        let left = if snapshotter.captured {
            Dirty::Paths(snapshotter.unreadable.clone())
        } else {
            due.dirty
        };
        let held = snapshotter.captured && !snapshotter.unreadable.is_empty();
        let attempt = Attempt {
            left,
            held,
            journal_only,
        };
        (snapshotter, result, attempt)
    }

    async fn snapshot(
        &mut self,
        snapshotter: Snapshotter,
        dirty: Dirty,
        journal_only: bool,
    ) -> (Snapshotter, Result<SnapshotReport, Error>) {
        if journal_only {
            return journal_snapshot(snapshotter, dirty).await;
        }
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
    /// Only held files are due: no write came since they were skipped.
    reread_only: bool,
}

impl Pending {
    fn new(dirty: Dirty, now: Instant) -> Option<Self> {
        Some(Self {
            dirty,
            first: now,
            last: now,
            not_before: None,
            reread_only: false,
        })
    }

    /// Paths to try again after `wait`. Events that arrived meanwhile are
    /// still queued and join it as they are read.
    fn retry(dirty: Dirty, wait: Duration) -> Option<Self> {
        let now = Instant::now();
        Some(Self {
            dirty,
            first: now,
            last: now,
            not_before: Some(now + wait),
            reread_only: false,
        })
    }

    /// Held files to read again after `wait`.
    fn reread(dirty: Dirty, wait: Duration) -> Option<Self> {
        Self::retry(dirty, wait).map(|pending| Self {
            reread_only: true,
            ..pending
        })
    }

    fn due(&self) -> Instant {
        let due = (self.last + QUIET).min(self.first + MAX_WAIT);
        self.not_before.map_or(due, |floor| due.max(floor))
    }

    fn add(&mut self, more: &Dirty, at: Instant) {
        self.last = at;
        // A new write is captured on the usual schedule, durably in the
        // journal if the server is still away, whatever the retry pause.
        self.not_before = None;
        self.reread_only = false;
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
    match err {
        // The journal waits for the change to reopen; retrying on a timer
        // would only ask the server the same question again.
        Error::JournalHalted { .. } => true,
        Error::Server(body) => matches!(body.code, ErrorCode::Refused | ErrorCode::NotFound),
        _ => false,
    }
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
