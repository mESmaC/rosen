//! Machine-local ready workspaces (SPEC R3). The daemon owns this manager.
use super::{layout::Layout, MaterialiseError};
use rosen_core::ObjectId;
use rosen_protocol::{ErrorBody, ErrorCode, Outcome, PooledWorkspace, Response};
use rosen_store::LooseObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, RwLock};

pub struct WarmPool {
    /// The daemon's data directory: its object store and workspace budget.
    /// The ready root may be elsewhere, on the volume workspaces live on.
    pub(super) data_dir: PathBuf,
    pub(super) state: Mutex<State>,
    /// The prepared template on a block-cloning volume. Its own lock lets a
    /// take clone from it while the worker refreshes ready slots; the worker
    /// writes its files only under the write lock or while it is `Running`,
    /// which takes never clone from.
    pub(super) template: Arc<RwLock<Option<Slot>>>,
    pub(super) wake: Notify,
}

pub(super) struct State {
    pub root: PathBuf,
    /// The target in force: `requested` when set, else the base's
    /// configured `warm_pool.size` (R3).
    pub size: usize,
    /// A `pool_size` request, such as Velocity raising N for a launch. It
    /// outranks the configuration until the daemon restarts.
    pub requested: Option<usize>,
    pub next: u64,
    pub ready: Vec<Slot>,
    /// The ready root's volume block clones, so ready workspaces are clones
    /// of one prepared template rather than each prepared on its own.
    pub block_clones: bool,
}

pub(super) struct Slot {
    pub path: PathBuf,
    pub tree: ObjectId,
    /// The base commit whose tree the slot holds.
    pub commit: ObjectId,
    pub files: u64,
    pub prep: Prep,
}

/// Where a workspace stands with the project's prepare commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Prep {
    /// The commands have not run since its last non-prose land.
    Needed,
    /// The commands are running in it now, so nothing may take or clone it.
    Running,
    /// Every command succeeded, at `commit` or before only prose landed.
    Done,
    /// A command failed at `commit`; retried when the base next moves.
    Failed,
}

impl WarmPool {
    /// A pool whose ready root is `<data_dir>/warm-pool`.
    pub fn new(data_dir: &Path) -> Arc<Self> {
        Self::with_root(data_dir, &data_dir.join("warm-pool"))
    }

    /// A pool whose ready root is `root`. A take renames a ready workspace
    /// into place, which works only on the root's volume, so the root
    /// belongs on the volume agents' workspaces live on (#86). The daemon
    /// removes `root` whole on start, so it must be the daemon's alone.
    pub fn with_root(data_dir: &Path, root: &Path) -> Arc<Self> {
        Arc::new(Self {
            data_dir: data_dir.to_path_buf(),
            state: Mutex::new(State {
                root: root.to_path_buf(),
                // Until the worker first reads the base's configuration.
                size: rosen_core::project_config::DEFAULT_WARM_POOL_SIZE as usize,
                requested: None,
                next: 0,
                ready: Vec::new(),
                block_clones: false,
            }),
            template: Arc::new(RwLock::new(None)),
            wake: Notify::new(),
        })
    }

    /// Runs until aborted by the daemon on shutdown. Reconnects resume by
    /// reading the current base, so missed events cannot strand old slots.
    pub fn start(
        self: &Arc<Self>,
        endpoint: rosen_protocol::Endpoint,
    ) -> tokio::task::JoinHandle<()> {
        let pool = Arc::clone(self);
        tokio::spawn(async move { super::pool_worker::run(pool, endpoint).await })
    }

    pub async fn set_size(&self, size: usize) -> Outcome {
        // The same limit the project configuration enforces, so a request
        // cannot make the worker copy the base tree until the disk fills.
        let limit = rosen_core::project_config::MAX_WARM_POOL_SIZE;
        if size > limit as usize {
            return Outcome::Error(ErrorBody::new(
                ErrorCode::InvalidArgument,
                format!("a warm pool size of {size} is over the limit of {limit}"),
            ));
        }
        let mut state = self.state.lock().await;
        state.size = size;
        state.requested = Some(size);
        drop(state);
        self.wake.notify_one();
        Outcome::Ok(Response::PoolSized)
    }

    /// Serves a take, in order of preference: a ready slot already holding
    /// `tree`, a block clone of the template at `tree`, or any other ready
    /// slot brought to `tree` by rewriting only the files that differ. The
    /// last keeps takes served while lands arrive faster than the worker
    /// refreshes the pool, as when a change starts from a base the worker
    /// has not yet followed.
    pub async fn take(self: &Arc<Self>, tree: ObjectId, dest: String) -> Outcome {
        let mut state = self.state.lock().await;
        let destination = Path::new(&dest);
        if !destination.is_absolute() || destination.starts_with(&state.root) {
            return Outcome::Error(ErrorBody::new(
                ErrorCode::InvalidArgument,
                "a pool destination must be absolute and outside the ready pool",
            ));
        }
        // A staging sweep would remove a workspace carrying the staging form.
        if destination
            .file_name()
            .is_some_and(super::staging_sweep::is_staging_name)
        {
            return Outcome::Error(ErrorBody::new(
                ErrorCode::InvalidArgument,
                "a pool destination may not have the name form Rosen reserves for staging",
            ));
        }
        if state.size == 0 {
            return Outcome::Ok(Response::PoolTaken(None));
        }
        let root = state.root.clone();
        while let Some(index) = pick(&state.ready, tree) {
            // Removing the slot under the same lock as refresh makes
            // ownership exclusive even when two clients ask together.
            let slot = state.ready.remove(index);
            let result = publish_slot(&slot, dest.clone(), &self.data_dir).await;
            if result.is_err() && vanished(&slot) {
                // Removed behind the daemon's back: re-queuing it would fail
                // every later take that draws it (#82). The worker refills.
                self.wake.notify_one();
                continue;
            }
            let prepared = slot.prep == Prep::Done;
            let files = slot.files;
            if result.is_err() {
                state.ready.push(slot);
            }
            drop(state);
            self.wake.notify_one();
            return taken(result, files, prepared, &root);
        }
        drop(state);
        let template = super::template::take_clone(&self.template, &self.data_dir, tree, &dest);
        match template.await {
            Outcome::Ok(Response::PoolTaken(None)) => self.take_stale(&root, tree, dest).await,
            served => served,
        }
    }

    /// Takes a ready slot holding another tree out of the pool and brings
    /// it to `tree` outside the pool lock, so other takes are not held up.
    /// It is reported unprepared: its prepare ran at another tree.
    async fn take_stale(&self, root: &Path, tree: ObjectId, dest: String) -> Outcome {
        let mut state = self.state.lock().await;
        let Some(index) = pick_stale(&state.ready) else {
            return Outcome::Ok(Response::PoolTaken(None));
        };
        let slot = state.ready.remove(index);
        drop(state);
        self.wake.notify_one();
        let Some(mut slot) = catch_up(&self.data_dir, slot, tree).await else {
            return Outcome::Ok(Response::PoolTaken(None));
        };
        let result = publish_slot(&slot, dest, &self.data_dir).await;
        if result.is_err() && vanished(&slot) {
            // Dropped, as in `take`; the client materialises without the pool.
            return Outcome::Ok(Response::PoolTaken(None));
        }
        let files = slot.files;
        if result.is_err() {
            // Its prepare ran at another tree; the worker refreshes it and
            // prepares it again before a take reports it prepared.
            slot.prep = Prep::Needed;
            self.state.lock().await.ready.push(slot);
        }
        taken(result, files, false, root)
    }
}

/// A slot whose directory is gone, as when an operator or antivirus removed
/// it. Checked only after its rename failed, so a healthy take pays nothing.
fn vanished(slot: &Slot) -> bool {
    std::fs::symlink_metadata(&slot.path)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn taken(result: std::io::Result<()>, files: u64, prepared: bool, root: &Path) -> Outcome {
    match result {
        Ok(()) => Outcome::Ok(Response::PoolTaken(Some(PooledWorkspace {
            files,
            prepared,
            from_template: false,
        }))),
        // Said in the answer, not only logged: on this layout every take
        // misses, and Velocity must see a pool that never serves (#86).
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            Outcome::Ok(Response::PoolMissed(format!(
                "the warm pool at {} is on another volume than the workspace; start \
                 the daemon with --warm-pool-dir or ROSEN_WARM_POOL_DIR set to a \
                 directory on the workspace's volume",
                root.display()
            )))
        }
        Err(error) => Outcome::Error(ErrorBody::new(
            ErrorCode::InvalidArgument,
            error.to_string(),
        )),
    }
}

/// A ready slot holding `tree`, prepared ones first. A slot whose prepare
/// is running is never offered: its commands are still writing into it.
fn pick(ready: &[Slot], tree: ObjectId) -> Option<usize> {
    let holds = |slot: &Slot| slot.tree == tree;
    ready
        .iter()
        .position(|slot| holds(slot) && slot.prep == Prep::Done)
        .or_else(|| {
            ready
                .iter()
                .position(|slot| holds(slot) && slot.prep != Prep::Running)
        })
}

/// Any ready slot a take may bring to another tree, prepared ones first:
/// their build output still spares most of a first build.
fn pick_stale(ready: &[Slot]) -> Option<usize> {
    ready
        .iter()
        .position(|slot| slot.prep == Prep::Done)
        .or_else(|| ready.iter().position(|slot| slot.prep != Prep::Running))
}

/// Brings a slot already removed from the ready list to `tree`. A slot
/// whose catch-up fails is deleted, never handed to an agent half-written.
async fn catch_up(data_dir: &Path, mut slot: Slot, tree: ObjectId) -> Option<Slot> {
    let objects = data_dir.join("objects");
    tokio::task::spawn_blocking(move || {
        let refreshed = LooseObjectStore::open(&objects)
            .map_err(MaterialiseError::Store)
            .and_then(|store| refresh(&store, &mut slot, tree));
        match refreshed {
            Ok(()) => Some(slot),
            Err(error) => {
                eprintln!("rosen warm pool: catching a slot up for a take: {error}");
                let _ = std::fs::remove_dir_all(&slot.path);
                None
            }
        }
    })
    .await
    .ok()
    .flatten()
}

async fn publish_slot(slot: &Slot, dest: String, data_dir: &Path) -> std::io::Result<()> {
    let source = slot.path.clone();
    let limits_root = data_dir.join("workspace-limits");
    tokio::task::spawn_blocking(move || publish(&source, Path::new(&dest), &limits_root))
        .await
        .unwrap_or_else(|error| Err(std::io::Error::other(error.to_string())))
}
fn publish(source: &Path, dest: &Path, limits_root: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(dest).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "workspace destination exists",
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::limits::Budget::open(limits_root, super::limits::Settings::default())?
        .rename_workspace(source, dest)
}

/// Rewrites only the tracked files that differ between the slot's tree and
/// `tree`. Untracked files, such as prepared build output, are kept, except
/// where `tree` now tracks a path: a slot or template holds no agent's work,
/// so output there is cleared rather than failing the refresh and losing
/// the prepared state with it (issue #236).
pub(super) fn refresh(
    store: &LooseObjectStore,
    slot: &mut Slot,
    tree: ObjectId,
) -> Result<(), MaterialiseError> {
    // Only the differing subtrees: walking both whole trees for every slot
    // after every land kept the pool behind a steady stream of lands.
    let (before, after) = Layout::diff(store, slot.tree, tree)?;
    // A prepare may have left a link where a tracked directory was. Every
    // step below resolves tracked paths through it, so it would clear and
    // write files outside the slot; the caller drops the slot (issue #312).
    super::clone::check_tracked_dirs(&slot.path, &before)?;
    super::clone::clear_for_tree(&slot.path, &before, &after)?;
    let changed = super::bring_to_tree(&slot.path, &before, &after)?;
    super::write_all(store, &slot.path, &changed)?;
    seed_slot(store, &slot.path, tree);
    slot.tree = tree;
    slot.files = (slot.files + after.files.len() as u64).saturating_sub(before.files.len() as u64);
    Ok(())
}

/// Rewrites from the store every tracked file under `root` whose content
/// differs from `tree` or is missing, and returns their paths. A prepare
/// command may write tracked files (codegen, a formatter, a lockfile
/// update); left there, the edit would reach the agent that takes the slot
/// and show up in its change as if it wrote it (issue #168). Build output,
/// which is untracked, is kept.
///
/// A link or file left where a tracked directory was is refused with
/// [`MaterialiseError::TrackedDirNotDir`] before anything is written, as
/// `refresh` does (#312): writing through it would change files outside
/// the slot. A tracked directory the prepare removed is made again (#322).
pub(super) fn restore_tracked(
    store: &LooseObjectStore,
    root: &Path,
    tree: ObjectId,
) -> Result<Vec<String>, MaterialiseError> {
    let layout = Layout::of_tree(store, tree, &super::Sparse::all())?;
    super::clone::check_tracked_dirs(root, &layout)?;
    // Parents come first, so each directory's parent already exists.
    for dir in &layout.dirs {
        let path = super::layout::disk_path(root, dir);
        match std::fs::create_dir(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(MaterialiseError::io(&path, error));
            }
            _ => {}
        }
    }
    let changed = std::sync::Mutex::new(Vec::new());
    super::parallel::for_each::<_, MaterialiseError, _>(&layout.files, |entry| {
        if !matches_tree(root, entry) {
            super::write::remove_file(root, &entry.path)?;
            super::write::write_entry(store, root, entry)?;
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            changed.lock().unwrap_or_else(|e| e.into_inner()).push(path);
        }
        Ok(())
    })?;
    let mut changed = changed.into_inner().unwrap_or_else(|e| e.into_inner());
    changed.sort();
    Ok(changed)
}

/// Whether the file on disk holds exactly the entry's blob. Unreadable or
/// missing counts as different, so the entry is written again.
fn matches_tree(root: &Path, entry: &super::layout::FileEntry) -> bool {
    let path = super::layout::disk_path(root, &entry.path);
    #[cfg(unix)]
    if entry.kind == super::layout::FileKind::Symlink {
        use std::os::unix::ffi::OsStrExt;
        return std::fs::read_link(&path).is_ok_and(|target| {
            ObjectId::hash_object("blob", target.as_os_str().as_bytes()) == entry.blob
        });
    }
    // A symlink on Windows is written as a plain file holding its target.
    std::fs::read(&path).is_ok_and(|data| ObjectId::hash_object("blob", &data) == entry.blob)
}

/// Fills a ready workspace's snapshot tree cache before any take, so the
/// take does not spend seconds writing thousands of trees (R3: ready in
/// under 2 s). A failure is only logged: the take seeds whatever is missing.
pub(super) fn seed_slot(store: &LooseObjectStore, path: &Path, tree: ObjectId) {
    if let Err(error) = crate::snapshot::seed_tree_cache(store, path, tree) {
        eprintln!("rosen warm pool: seeding {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosen_core::{FileMode, Tree, TreeEntry};
    use rosen_store::ObjectStore;

    // R3 critical contract: two simultaneous takes must not give agents the
    // same directory, where either agent could overwrite the other's work.
    // Covers a slot the worker refreshed and one a take must catch up.
    #[tokio::test]
    async fn concurrent_takes_transfer_a_slot_once() {
        two_takes_of_one_slot(true).await;
        two_takes_of_one_slot(false).await;
    }

    async fn two_takes_of_one_slot(refreshed: bool) {
        let dir = tempfile::tempdir().unwrap();
        let store = LooseObjectStore::open(dir.path().join("objects")).unwrap();
        let make_tree = |bytes: &[u8]| {
            let blob = store.put_blob(bytes).unwrap();
            store
                .put_tree(
                    &Tree::new(vec![
                        TreeEntry::new(FileMode::Regular, "file", blob).unwrap()
                    ])
                    .unwrap(),
                )
                .unwrap()
        };
        let before = make_tree(b"before");
        let after = make_tree(b"after");
        let pool = WarmPool::new(dir.path());
        let path = dir.path().join("ready");
        super::super::materialise(&store, before, &path, &super::super::Options::default())
            .unwrap();
        let mut slot = Slot {
            path,
            tree: before,
            commit: before,
            files: 1,
            prep: Prep::Needed,
        };
        if refreshed {
            refresh(&store, &mut slot, after).unwrap();
        }
        pool.state.lock().await.ready.push(slot);
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let (a, b) = tokio::join!(
            pool.take(after, first.display().to_string()),
            pool.take(after, second.display().to_string())
        );
        let transferred = |result| matches!(result, Outcome::Ok(Response::PoolTaken(Some(taken))) if taken.files == 1);
        assert_ne!(transferred(a), transferred(b));
        let taken = if first.exists() { first } else { second };
        assert_eq!(std::fs::read(taken.join("file")).unwrap(), b"after");
    }
}
