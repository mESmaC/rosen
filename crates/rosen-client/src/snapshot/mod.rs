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

mod ignore;
mod index;
mod marker;
mod scan;
mod upload;
pub mod watch;

pub use marker::{WorkspaceMarker, STATE_DIR};
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
        let index = Index::from_tree(store, marker.tree)?;
        let sparse = Sparse::paths(&marker.sparse).map_err(|err| {
            SnapshotError::io(
                &WorkspaceMarker::path(&root),
                std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()),
            )
        })?;
        let known_trees = index.build_trees()?.trees.into_iter().map(|(id, _)| id);
        Ok(Self {
            ignore: ignore::IgnoreRules::new(root.clone()),
            sparse,
            known_trees: known_trees.chain([marker.tree]).collect(),
            root,
            marker,
            index,
            unconfirmed: BTreeSet::new(),
        })
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
        let trees = built
            .trees
            .into_iter()
            .filter(|(id, _)| !self.known_trees.contains(id))
            .collect();
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
}

/// Takes one snapshot of `dirty` paths. The snapshotter is handed back
/// whatever happens, so a caller can retry.
pub async fn take_snapshot(
    client: &Client,
    mut snapshotter: Snapshotter,
    dirty: Dirty,
    message: Option<String>,
) -> (Snapshotter, Result<SnapshotReport, Error>) {
    let joined = tokio::task::spawn_blocking(move || {
        let prepared = snapshotter.prepare(&dirty);
        (snapshotter, prepared)
    })
    .await;
    let (mut snapshotter, prepared) = match joined {
        Ok(pair) => pair,
        Err(join) => std::panic::resume_unwind(join.into_panic()),
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(err) => return (snapshotter, Err(err.into())),
    };
    let result = send(client, &mut snapshotter, &prepared, message).await;
    if result.is_err() {
        snapshotter.fail(&prepared);
    }
    (snapshotter, result)
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
    report.sent = upload::send_objects(client, &prepared.blobs, &prepared.trees).await?;
    let outcome = client
        .snapshot(SnapshotRequest {
            change: snapshotter.change().to_string(),
            tree: prepared.root,
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
