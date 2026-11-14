//! Write-through snapshots (SPEC R2, D17): turning a local workspace's files
//! into snapshots on the server, sending only what changed.
//!
//! A [`Snapshotter`] keeps the workspace's index (each tracked file's mode,
//! blob and last seen size and time). A snapshot scans the paths that may
//! have changed, reads and hashes those files, rebuilds the trees, and sends
//! the server the changed blobs and the trees it has not seen, then names the
//! root tree in a `snapshot` request. [`watch`] runs this from file system
//! events (`ReadDirectoryChangesW` on Windows); `rosen commit` runs it once
//! with a message.

mod guard;
mod ignore;
mod index;
mod journal;
mod marker;
mod rebase;
mod scan;
mod seed;
mod stat_cache;
mod upload;
pub mod watch;

pub use journal::pending_journal;
pub use marker::{WorkspaceMarker, STATE_DIR};
pub use seed::seed_tree_cache;
pub(crate) use stat_cache::seed as seed_stat_cache;
pub use upload::Sent;
pub use watch::{watch, WatchEvent};

use crate::connection::unexpected;
use crate::workspace::Sparse;
use crate::{Client, Error};
use index::Index;
use rosen_core::{ChangeId, ObjectId, SnapshotKind};
use rosen_protocol::{Request, Response, SnapshotOutcome, SnapshotRequest};
use rosen_store::{ObjectStore, StoreError};
use scan::ScanOutcome;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use upload::ChangedBlob;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("{0} is not inside a Rosen workspace (no .rosen/workspace.json above it)")]
    NoWorkspace(PathBuf),
    #[error("the stored tree holds a path that is not UTF-8: {:?}", String::from_utf8_lossy(.0))]
    NonUtf8Path(Vec<u8>),
    #[error(transparent)]
    Core(#[from] rosen_core::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl SnapshotError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Which paths a snapshot must look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dirty {
    /// Walk the whole workspace.
    Full,
    /// Only these `/`-separated paths from the root, as the watcher reported.
    Paths(BTreeSet<String>),
}

/// What one snapshot did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotReport {
    pub change: ChangeId,
    /// The workspace's tree, now stored on the server.
    pub tree: ObjectId,
    /// The snapshot commit, or the existing one when nothing was recorded.
    pub commit: Option<ObjectId>,
    /// False when the server already held this tree as the latest snapshot.
    pub recorded: bool,
    /// Files whose blob was sent.
    pub changed_files: Vec<String>,
    /// Files no longer in the workspace.
    pub removed: usize,
    /// Files another process held open, so the snapshot kept their last
    /// stored content. Only a watcher's snapshots skip them.
    pub unreadable: Vec<String>,
    pub sent: Sent,
    /// Offline journal records stored ahead of this snapshot, oldest first,
    /// one report each.
    pub replayed: Vec<SnapshotReport>,
}

/// A local workspace's snapshot state.
pub struct Snapshotter {
    root: PathBuf,
    journal: journal::Journal,
    marker: WorkspaceMarker,
    index: Index,
    ignore: ignore::IgnoreRules,
    sparse: Sparse,
    /// Trees the server is known to hold, so they are never sent again.
    known_trees: HashSet<ObjectId>,
    /// Files whose blob was read but whose snapshot failed. They are sent
    /// again with the next one, even if unchanged since.
    unconfirmed: BTreeSet<String>,
    /// Whether a file another process holds open is skipped rather than
    /// failing the snapshot. The watcher sets it and reads such a file
    /// again later; `rosen commit` names the file and fails instead.
    defer_unreadable: bool,
    /// The files the last prepared snapshot skipped.
    unreadable: BTreeSet<String>,
    /// Whether the last attempt's snapshot is durable: stored on the server
    /// or in the journal. A failed attempt that was captured needs only a
    /// replay on retry, not another scan of what it already read.
    captured: bool,
    /// The tree this snapshotter last captured, or opened at. When the
    /// workspace's newest captured tree is another, a second writer such as
    /// `rosen commit` captured since, and the next scan must be full (#93).
    captured_root: ObjectId,
    /// The last root tree whose objects the server took. The server keeps
    /// every object (D5) and takes a tree only once all it names is stored,
    /// so a retry of this tree, as after an internal server error, sends
    /// only the snapshot request again rather than every blob (#93).
    uploaded_root: Option<ObjectId>,
}

/// A snapshot read from disk and ready to send.
#[derive(serde::Serialize, serde::Deserialize)]
struct Prepared {
    blobs: Vec<ChangedBlob>,
    trees: Vec<(ObjectId, Vec<u8>)>,
    root: ObjectId,
    all_trees: Vec<ObjectId>,
    removed: usize,
}

impl Snapshotter {
    /// Opens the workspace at `root` from its marker. Blocking: it reads the
    /// last stored tree from the machine's local store.
    pub fn open(store: &dyn ObjectStore, root: &Path) -> Result<Self, SnapshotError> {
        let root = std::path::absolute(root).map_err(|err| SnapshotError::io(root, err))?;
        let marker = WorkspaceMarker::read(&root).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => SnapshotError::NoWorkspace(root.clone()),
            _ => SnapshotError::io(&WorkspaceMarker::path(&root), err),
        })?;
        let journal = journal::Journal::open(&root).map_err(|e| SnapshotError::io(&root, e))?;
        let (mut index, known_trees, captured_root) = {
            // Another writer may be appending or replaying; read its tail
            // only once it is done.
            let _writer = journal.writer().map_err(|e| SnapshotError::io(&root, e))?;
            open_index(store, &root, &journal, marker.tree)?
        };
        stat_cache::apply(&root, &mut index);
        let sparse = Sparse::paths(&marker.sparse).map_err(|err| {
            SnapshotError::io(
                &WorkspaceMarker::path(&root),
                std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()),
            )
        })?;
        Ok(Self {
            ignore: ignore::IgnoreRules::new(root.clone(), &marker.ignore),
            sparse,
            known_trees,
            root,
            journal,
            marker,
            index,
            unconfirmed: BTreeSet::new(),
            defer_unreadable: false,
            unreadable: BTreeSet::new(),
            captured: false,
            captured_root,
            uploaded_root: None,
        })
    }

    /// Opens a previously materialised workspace without contacting the server.
    pub fn open_offline(root: &Path) -> Result<Self, SnapshotError> {
        Self::open(&journal::cache(root)?, root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn change(&self) -> ChangeId {
        self.marker.change
    }

    /// The tree last stored on the server for this workspace.
    pub fn stored_tree(&self) -> ObjectId {
        self.marker.tree
    }

    /// Makes later snapshots skip, and report, a file another process holds
    /// open instead of failing, so one held file never delays the others.
    pub fn defer_unreadable(&mut self) {
        self.defer_unreadable = true;
    }

    /// Whether a write to `rel` can change the next snapshot. Writes to
    /// ignored paths do not, so they never delay one (SPEC R2).
    pub fn is_relevant(&mut self, rel: &str) -> bool {
        if ignore::always_ignored(rel) {
            return false;
        }
        ignore::is_ignore_file(rel)
            || self.index.tracks(rel)
            || self.index.tracks_under(rel)
            || !self
                .ignore
                .is_ignored(rel, scan::disk(&self.root, rel).is_dir())
    }

    /// Scans, reads and builds trees. Blocking.
    fn prepare(&mut self, dirty: &Dirty) -> Result<Prepared, SnapshotError> {
        let mut outcome = ScanOutcome::default();
        self.scan(dirty, &mut outcome)?;
        let mut full = *dirty == Dirty::Full;
        if outcome.ignore_changed && !full {
            // New rules can un-ignore files nobody wrote to since.
            self.scan(&Dirty::Full, &mut outcome)?;
            full = true;
        }
        if outcome.ignore_changed {
            scan::drop_ignored(&mut self.ignore, &self.index, &mut outcome);
        }
        let read = upload::read_candidates(
            &self.root,
            &mut self.index,
            &outcome.candidates,
            &self.unconfirmed,
            self.defer_unreadable,
        )?;
        self.unreadable = read.unreadable;
        if full {
            // Only a speed-up for the next snapshotter's full scan, so a
            // failed save just leaves it rereading files.
            let _ = stat_cache::save(&self.root, &self.index);
        }
        let built = self.index.build_trees()?;
        let all_trees = built.trees.iter().map(|(id, _)| *id).collect();
        let trees = built.trees.into_iter().collect();
        Ok(Prepared {
            blobs: read.changed,
            trees,
            root: built.root,
            all_trees,
            removed: outcome.removed,
        })
    }

    fn scan(&mut self, dirty: &Dirty, outcome: &mut ScanOutcome) -> Result<(), SnapshotError> {
        let mut view = scan::View {
            root: &self.root,
            ignore: &mut self.ignore,
            index: &mut self.index,
            sparse: &self.sparse,
        };
        match dirty {
            Dirty::Full => scan::scan_dir(&mut view, "", outcome)
                .map_err(|err| SnapshotError::io(&self.root, err)),
            Dirty::Paths(paths) => paths.iter().try_for_each(|rel| {
                scan::scan_path(&mut view, rel, outcome)
                    .map_err(|err| SnapshotError::io(&scan::disk(&self.root, rel), err))
            }),
        }
    }

    /// Records a stored snapshot: its trees are known from now on, and the
    /// marker names its tree for the next watcher or `rosen commit`.
    fn confirm(&mut self, prepared: &Prepared) -> Result<(), SnapshotError> {
        self.known_trees.extend(prepared.all_trees.iter().copied());
        self.marker.tree = prepared.root;
        self.marker
            .write(&self.root)
            .map_err(|err| SnapshotError::io(&WorkspaceMarker::path(&self.root), err))
    }

    /// Takes the writer lock, then prepares a snapshot. Blocking.
    fn lease_and_prepare(
        &mut self,
        dirty: &Dirty,
    ) -> Result<(journal::WriterLease, Prepared), SnapshotError> {
        let lease = self
            .journal
            .writer()
            .map_err(|err| SnapshotError::io(&self.root, err))?;
        // Another writer may have stored a snapshot since this one last ran.
        let path = WorkspaceMarker::path(&self.root);
        self.marker.tree = WorkspaceMarker::read(&self.root)
            .map_err(|err| SnapshotError::io(&path, err))?
            .tree;
        // That writer may have read files whose events this one has not had
        // yet. Building on this index's older view of them would store them
        // reverted, so look at every file. A full scan rereads only files
        // whose size or time moved since this index read them.
        let dirty = if self.newest_captured()? == self.captured_root {
            dirty
        } else {
            &Dirty::Full
        };
        Ok((lease, self.prepare(dirty)?))
    }

    /// The newest tree any writer captured: the last journal record's, or
    /// the stored one. Call while holding the writer lock.
    fn newest_captured(&self) -> Result<ObjectId, SnapshotError> {
        let io = |err| SnapshotError::io(&self.root, err);
        match self.journal.pending().map_err(io)?.last() {
            Some(&last) => self.journal.root_of(last).map_err(io),
            None => Ok(self.marker.tree),
        }
    }

    fn journal_record(
        &mut self,
        last: Option<u64>,
        prepared: Prepared,
        message: Option<String>,
        unanswered: bool,
    ) -> Result<(), SnapshotError> {
        journal::save_trees(&self.root, &prepared)?;
        let record = journal::Record {
            prepared,
            message,
            unanswered,
        };
        self.journal.append(last, &record)
    }

    /// A report of `prepared`, not yet stored.
    fn report(&self, prepared: &Prepared) -> SnapshotReport {
        SnapshotReport {
            change: self.change(),
            tree: prepared.root,
            commit: None,
            recorded: false,
            changed_files: prepared.blobs.iter().map(|b| b.path.clone()).collect(),
            removed: prepared.removed,
            unreadable: Vec::new(),
            sent: Sent::default(),
            replayed: Vec::new(),
        }
    }

    /// Takes the workspace for deletion (SPEC R4). Fails while any other
    /// snapshotter, such as a running watcher, has it open.
    pub(crate) fn hold_for_deletion(&self) -> Result<(), SnapshotError> {
        self.journal
            .exclusive()
            .map_err(|err| SnapshotError::io(&self.root, err))
    }
}

/// Builds the index from the last journal record, or from the stored tree
/// when nothing waits, and caches the stored tree's trees so a watcher can
/// start offline. Returns the trees the server is known to hold, and the
/// tree the index holds.
fn open_index(
    store: &dyn ObjectStore,
    root: &Path,
    journal: &journal::Journal,
    stored: ObjectId,
) -> Result<(Index, HashSet<ObjectId>, ObjectId), SnapshotError> {
    let index = Index::from_tree(store, stored)?;
    seed_tree_cache(store, root, stored)?;
    let cache = journal::cache(root)?;
    let mut known_trees = HashSet::from([stored]);
    known_trees.extend(index.build_trees()?.trees.into_iter().map(|(id, _)| id));
    let io = |err| SnapshotError::io(root, err);
    let Some(last) = journal.pending().map_err(io)?.last().copied() else {
        return Ok((index, known_trees, stored));
    };
    let last_root = journal.root_of(last).map_err(io)?;
    if !cache.contains(&last_root)? {
        // Records from an earlier build carried their trees inline.
        journal::save_trees(root, &journal.load(last, &cache)?.prepared)?;
    }
    let index = Index::from_tree(&cache, last_root)?;
    Ok((index, known_trees, last_root))
}

/// Takes one snapshot of `dirty` paths. The snapshotter is handed back
/// whatever happens, so a caller can retry.
pub async fn take_snapshot(
    client: &Client,
    snapshotter: Snapshotter,
    dirty: Dirty,
    message: Option<String>,
) -> (Snapshotter, Result<SnapshotReport, Error>) {
    capture(Some(client), snapshotter, dirty, message).await
}

/// Captures a snapshot durably while no connection is available.
pub async fn journal_snapshot(
    snapshotter: Snapshotter,
    dirty: Dirty,
) -> (Snapshotter, Result<SnapshotReport, Error>) {
    capture(None, snapshotter, dirty, None).await
}

/// Prepares and stores one snapshot while holding the journal's writer lock,
/// so a second writer (`rosen commit` beside a watcher) waits its turn and
/// then scans what this one left.
async fn capture(
    client: Option<&Client>,
    mut snapshotter: Snapshotter,
    dirty: Dirty,
    message: Option<String>,
) -> (Snapshotter, Result<SnapshotReport, Error>) {
    snapshotter.captured = false;
    let joined = tokio::task::spawn_blocking(move || {
        let prepared = snapshotter.lease_and_prepare(&dirty);
        (snapshotter, prepared)
    })
    .await;
    let (mut snapshotter, prepared) = match joined {
        Ok(pair) => pair,
        Err(join) => std::panic::resume_unwind(join.into_panic()),
    };
    let (lease, prepared) = match prepared {
        Ok(pair) => pair,
        Err(err) => return (snapshotter, Err(err.into())),
    };
    let result = queue_and_replay(client, &mut snapshotter, prepared, message)
        .await
        .map(|mut report| {
            report.unreadable = snapshotter.unreadable.iter().cloned().collect();
            report
        });
    drop(lease);
    (snapshotter, result)
}

/// Stores a prepared snapshot, then settles which read files must be sent
/// again: none once the snapshot is captured, since its blobs are on the
/// server or in the journal; otherwise every blob it read, as the index
/// already holds their ids.
async fn queue_and_replay(
    client: Option<&Client>,
    state: &mut Snapshotter,
    prepared: Prepared,
    message: Option<String>,
) -> Result<SnapshotReport, Error> {
    let read: Vec<String> = prepared.blobs.iter().map(|b| b.path.clone()).collect();
    let root = prepared.root;
    let result = store_or_queue(client, state, prepared, message).await;
    if state.captured {
        // Every unconfirmed path was read into this snapshot's blobs.
        state.unconfirmed.clear();
        state.captured_root = root;
    } else {
        state.unconfirmed.extend(read);
    }
    result
}

/// Sends a snapshot straight to the server when nothing waits in the
/// journal. The journal is written only when the server cannot be reached,
/// or behind earlier records so the order holds (SPEC R2).
async fn store_or_queue(
    client: Option<&Client>,
    state: &mut Snapshotter,
    prepared: Prepared,
    message: Option<String>,
) -> Result<SnapshotReport, Error> {
    let pending = state.journal.pending()?;
    if let (Some(client), true) = (client, pending.is_empty()) {
        let result = send(client, state, &prepared, message.clone()).await;
        if let Err(err) = &result {
            if !worth_journaling(err) {
                return result;
            }
            // Without the server's answer, a named snapshot may be stored.
            let unanswered = message.is_some() && !matches!(err, Error::Server(_));
            state.journal_record(None, prepared, message, unanswered)?;
        }
        state.captured = true;
        return result;
    }
    let last = pending.last().copied();
    let previous = match last {
        Some(id) => state.journal.root_of(id)?,
        None => state.marker.tree,
    };
    // An unchanged tree is already held by the last record, or the server.
    if prepared.root != previous || message.is_some() {
        state.journal_record(last, prepared, message, false)?;
    }
    state.captured = true;
    let client = client.ok_or(Error::Disconnected)?;
    if let Some(halt) = state.journal.halted()? {
        if !may_resume(client, state.change(), halt.seen).await? {
            return Err(Error::JournalHalted {
                record: halt.record,
                refusal: halt.refusal,
            });
        }
        // A post-land revert (R12) reopened the change, or a server rebase
        // gave it a new head: its records may be stored now.
        state.journal.resume()?;
    }
    replay(client, state).await
}

/// Whether a halted journal may replay: the change is open and is not as it
/// was when the server refused the record. One small request, so a halted
/// journal never uploads a record's blobs only to have them refused again:
/// an unchanged open change gives the same answer to the same request
/// (#273), and each new snapshotter, such as each `rosen commit`, would
/// upload them anew.
async fn may_resume(
    client: &Client,
    change: ChangeId,
    halted: Option<journal::ChangeSeen>,
) -> Result<bool, Error> {
    Ok(match change_seen(client, change).await? {
        Some(now) => !now.state.is_closed() && halted != Some(now),
        None => false,
    })
}

/// The change's state and head now, or `None` when the server holds no such
/// change.
async fn change_seen(
    client: &Client,
    change: ChangeId,
) -> Result<Option<journal::ChangeSeen>, Error> {
    match client.change_show(&change.to_string()).await {
        Ok(record) => Ok(Some(journal::ChangeSeen {
            state: record.change.state,
            head: record.change.head,
        })),
        Err(Error::Server(body)) if body.code == rosen_protocol::ErrorCode::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Uploads every journal record in order, removing each once stored. The
/// last record's report carries the earlier ones. A record the server
/// refuses for good halts the journal instead of failing every later
/// snapshot on it again: it stays, with all after it (issue #91).
async fn replay(client: &Client, state: &mut Snapshotter) -> Result<SnapshotReport, Error> {
    let trees = journal::cache(&state.root)?;
    let mut reports = Vec::new();
    let pending = state.journal.pending()?;
    // Read before any record is sent, so a halt records the change as it
    // stood no later than the refusal. Read after, it could already show a
    // server rebase made in between, and the halt would wait for a further
    // move. A move before the refusal costs only one more replay. Unread,
    // the halt resumes whenever the change is open.
    let seen = if pending.is_empty() {
        None
    } else {
        change_seen(client, state.change()).await.ok().flatten()
    };
    for id in pending {
        let record = state.journal.load(id, &trees)?;
        match replay_record(client, state, id, record).await {
            Ok(report) => reports.push(report),
            Err(Error::Server(refusal)) if !transient(refusal.code) => {
                state.journal.halt(id, &refusal, seen)?;
                return Err(Error::JournalHalted {
                    record: id,
                    refusal,
                });
            }
            Err(err) => return Err(err),
        }
        state.journal.remove(id)?;
    }
    // Drops the blob store once the last record is gone.
    state.journal.pending()?;
    let mut last = reports.pop().unwrap_or_else(|| SnapshotReport {
        change: state.change(),
        tree: state.marker.tree,
        commit: None,
        recorded: false,
        changed_files: Vec::new(),
        removed: 0,
        unreadable: Vec::new(),
        sent: Sent::default(),
        replayed: Vec::new(),
    });
    last.replayed = reports;
    Ok(last)
}

/// Sends one journal record. A named record whose earlier request may have
/// been stored without the answer arriving is first looked up, so the
/// commit is not recorded twice (#93). An automatic record needs no lookup:
/// the server stores nothing for the tree its change already holds.
async fn replay_record(
    client: &Client,
    state: &mut Snapshotter,
    id: u64,
    record: journal::Record,
) -> Result<SnapshotReport, Error> {
    let Some(message) = record.message else {
        return send(client, state, &record.prepared, None).await;
    };
    if record.unanswered {
        let shown = client.change_show(&state.change().to_string()).await?;
        let named = SnapshotKind::Named(message.clone());
        let latest = shown.change.snapshots.last();
        if let Some(stored) = latest.filter(|s| s.tree == record.prepared.root && s.kind == named) {
            state.confirm(&record.prepared)?;
            let mut report = state.report(&record.prepared);
            report.commit = Some(stored.commit);
            report.recorded = true;
            return Ok(report);
        }
    } else {
        // Past this point the server may store it with the answer lost,
        // or this process may stop before hearing it.
        state.journal.mark_unanswered(id)?;
    }
    send(client, state, &record.prepared, Some(message)).await
}

/// Failures the server may not have seen, so the snapshot is kept for a
/// later replay. A refusal is final: journaling it would stall every later
/// snapshot behind a record the server never takes.
fn worth_journaling(err: &Error) -> bool {
    match err {
        Error::Disconnected | Error::NotRunning(_) | Error::Io(_) | Error::Start(_) => true,
        Error::Server(body) => transient(body.code),
        _ => false,
    }
}

/// Server errors a later attempt may not see again. Any other answer is the
/// server's final word on the request.
fn transient(code: rosen_protocol::ErrorCode) -> bool {
    use rosen_protocol::ErrorCode;
    matches!(
        code,
        ErrorCode::Unavailable | ErrorCode::ShuttingDown | ErrorCode::Internal
    )
}

async fn send(
    client: &Client,
    snapshotter: &mut Snapshotter,
    prepared: &Prepared,
    message: Option<String>,
) -> Result<SnapshotReport, Error> {
    let unchanged = prepared.root == snapshotter.marker.tree && prepared.blobs.is_empty();
    let mut report = snapshotter.report(prepared);
    if unchanged && message.is_none() {
        return Ok(report);
    }
    if snapshotter.uploaded_root != Some(prepared.root) {
        let trees = prepared
            .trees
            .iter()
            .filter(|(id, _)| !snapshotter.known_trees.contains(id))
            .cloned()
            .collect::<Vec<_>>();
        report.sent = upload::send_objects(client, &prepared.blobs, &trees).await?;
        snapshotter.uploaded_root = Some(prepared.root);
    }
    let outcome = client
        .snapshot(SnapshotRequest {
            change: snapshotter.change().to_string(),
            tree: prepared.root,
            from: Some(snapshotter.marker.tree),
            message,
        })
        .await?;
    snapshotter.confirm(prepared)?;
    report.commit = outcome.commit;
    report.recorded = outcome.recorded;
    Ok(report)
}

impl Client {
    /// Records a snapshot of a tree whose objects the server holds.
    pub async fn snapshot(&self, request: SnapshotRequest) -> Result<SnapshotOutcome, Error> {
        match self.request(Request::Snapshot(request)).await? {
            Response::Snapshot(outcome) => Ok(outcome),
            other => Err(unexpected(other)),
        }
    }

    /// Opens the workspace at `root` for snapshots.
    pub async fn open_snapshotter(&self, root: &Path) -> Result<Snapshotter, Error> {
        // The last stored tree may have been uploaded from another machine's
        // workspace or rebased on the server, so make it local first (D8).
        if let Ok(marker) = WorkspaceMarker::read(root) {
            self.ensure_sparse_local(marker.tree, marker.sparse).await?;
        }
        let store = self.local_store().await?;
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || Snapshotter::open(&store, &root))
            .await
            .map_err(|err| Error::Unexpected(format!("workspace open stopped: {err}")))?
            .map_err(Error::from)
    }

    /// Scans the whole workspace and stores a snapshot of it: what
    /// `rosen commit` does. With a message, a snapshot is recorded even when
    /// nothing changed, so the message is never lost.
    pub async fn snapshot_workspace(
        &self,
        root: &Path,
        message: Option<String>,
    ) -> Result<SnapshotReport, Error> {
        let snapshotter = self.open_snapshotter(root).await?;
        take_snapshot(self, snapshotter, Dirty::Full, message)
            .await
            .1
    }
}
