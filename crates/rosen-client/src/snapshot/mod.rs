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
mod upload;
pub mod watch;

pub use journal::pending_journal;
pub use marker::{WorkspaceMarker, STATE_DIR};
pub use seed::seed_tree_cache;
pub use upload::{Sent, MAX_FILE_BYTES};
pub use watch::{watch, WatchEvent};

use crate::connection::unexpected;
use crate::workspace::Sparse;
use crate::{Client, Error};
use index::Index;
use rosen_core::{ChangeId, ObjectId};
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
    #[error("{0} is {1} bytes; a snapshot carries files up to {MAX_FILE_BYTES} bytes")]
    TooLarge(String, u64),
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
    pub sent: Sent,
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
        let (index, known_trees) = {
            // Another writer may be appending or replaying; read its tail
            // only once it is done.
            let _writer = journal.writer().map_err(|e| SnapshotError::io(&root, e))?;
            open_index(store, &root, &journal, marker.tree)?
        };
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
        if outcome.ignore_changed && *dirty != Dirty::Full {
            // New rules can un-ignore files nobody wrote to since.
            self.scan(&Dirty::Full, &mut outcome)?;
        }
        let blobs = upload::read_candidates(
            &self.root,
            &mut self.index,
            &outcome.candidates,
            &self.unconfirmed,
        )?;
        let built = self.index.build_trees()?;
        let all_trees = built.trees.iter().map(|(id, _)| *id).collect();
        let trees = built.trees.into_iter().collect();
        Ok(Prepared {
            blobs,
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
        self.unconfirmed.clear();
        self.marker.tree = prepared.root;
        self.marker
            .write(&self.root)
            .map_err(|err| SnapshotError::io(&WorkspaceMarker::path(&self.root), err))
    }

    fn fail(&mut self, prepared: &Prepared) {
        let paths = prepared.blobs.iter().map(|blob| blob.path.clone());
        self.unconfirmed.extend(paths);
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
        Ok((lease, self.prepare(dirty)?))
    }

    fn journal_record(
        &mut self,
        last: Option<u64>,
        prepared: Prepared,
        message: Option<String>,
    ) -> Result<(), SnapshotError> {
        journal::save_trees(&self.root, &prepared)?;
        self.journal
            .append(last, &journal::Record { prepared, message })
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
/// start offline. Returns the trees the server is known to hold.
fn open_index(
    store: &dyn ObjectStore,
    root: &Path,
    journal: &journal::Journal,
    stored: ObjectId,
) -> Result<(Index, HashSet<ObjectId>), SnapshotError> {
    let index = Index::from_tree(store, stored)?;
    seed_tree_cache(store, root, stored)?;
    let cache = journal::cache(root)?;
    let mut known_trees = HashSet::from([stored]);
    known_trees.extend(index.build_trees()?.trees.into_iter().map(|(id, _)| id));
    let io = |err| SnapshotError::io(root, err);
    let Some(last) = journal.pending().map_err(io)?.last().copied() else {
        return Ok((index, known_trees));
    };
    let last_root = journal.root_of(last).map_err(io)?;
    if !cache.contains(&last_root)? {
        // Records from an earlier build carried their trees inline.
        journal::save_trees(root, &journal.load(last, &cache)?.prepared)?;
    }
    Ok((Index::from_tree(&cache, last_root)?, known_trees))
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
    let result = queue_and_replay(client, &mut snapshotter, prepared, message).await;
    drop(lease);
    (snapshotter, result)
}

/// Sends a snapshot straight to the server when nothing waits in the
/// journal. The journal is written only when the server cannot be reached,
/// or behind earlier records so the order holds (SPEC R2).
async fn queue_and_replay(
    client: Option<&Client>,
    state: &mut Snapshotter,
    prepared: Prepared,
    message: Option<String>,
) -> Result<SnapshotReport, Error> {
    state.fail(&prepared);
    let pending = state.journal.pending()?;
    if let (Some(client), true) = (client, pending.is_empty()) {
        return match send(client, state, &prepared, message.clone()).await {
            Err(err) if worth_journaling(&err) => {
                state.journal_record(None, prepared, message)?;
                Err(err)
            }
            result => result,
        };
    }
    let last = pending.last().copied();
    let previous = match last {
        Some(id) => state.journal.root_of(id)?,
        None => state.marker.tree,
    };
    if prepared.root != previous || message.is_some() {
        state.journal_record(last, prepared, message)?;
    }
    replay(client.ok_or(Error::Disconnected)?, state).await
}

/// Uploads every journal record in order, removing each once stored.
async fn replay(client: &Client, state: &mut Snapshotter) -> Result<SnapshotReport, Error> {
    let trees = journal::cache(&state.root)?;
    let mut report = SnapshotReport {
        change: state.change(),
        tree: state.marker.tree,
        commit: None,
        recorded: false,
        changed_files: Vec::new(),
        removed: 0,
        sent: Sent::default(),
    };
    for id in state.journal.pending()? {
        let record = state.journal.load(id, &trees)?;
        report = send(client, state, &record.prepared, record.message).await?;
        state.journal.remove(id)?;
    }
    // Drops the blob store once the last record is gone.
    state.journal.pending()?;
    Ok(report)
}

/// Failures the server may not have seen, so the snapshot is kept for a
/// later replay. A refusal is final: journaling it would stall every later
/// snapshot behind a record the server never takes.
fn worth_journaling(err: &Error) -> bool {
    use rosen_protocol::ErrorCode;
    match err {
        Error::Disconnected | Error::NotRunning(_) | Error::Io(_) | Error::Start(_) => true,
        Error::Server(body) => matches!(
            body.code,
            ErrorCode::Unavailable | ErrorCode::ShuttingDown | ErrorCode::Internal
        ),
        _ => false,
    }
}

async fn send(
    client: &Client,
    snapshotter: &mut Snapshotter,
    prepared: &Prepared,
    message: Option<String>,
) -> Result<SnapshotReport, Error> {
    let unchanged = prepared.root == snapshotter.marker.tree && prepared.blobs.is_empty();
    let mut report = SnapshotReport {
        change: snapshotter.change(),
        tree: prepared.root,
        commit: None,
        recorded: false,
        changed_files: prepared
            .blobs
            .iter()
            .map(|blob| blob.path.clone())
            .collect(),
        removed: prepared.removed,
        sent: Sent::default(),
    };
    if unchanged && message.is_none() {
        return Ok(report);
    }
    let trees = prepared
        .trees
        .iter()
        .filter(|(id, _)| !snapshotter.known_trees.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    report.sent = upload::send_objects(client, &prepared.blobs, &trees).await?;
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
