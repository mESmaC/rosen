//! Machine-local ready workspaces (SPEC R3). The daemon owns this manager.
use super::{layout::Layout, MaterialiseError, Sparse};
use rosen_core::ObjectId;
use rosen_protocol::{ErrorBody, ErrorCode, Outcome, PooledWorkspace, Response};
use rosen_store::LooseObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, RwLock};

pub struct WarmPool {
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
    pub fn new(data_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                root: data_dir.join("warm-pool"),
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
        let mut state = self.state.lock().await;
        state.size = size;
        state.requested = Some(size);
        drop(state);
        self.wake.notify_one();
        Outcome::Ok(Response::PoolSized)
    }

    pub async fn take(self: &Arc<Self>, tree: ObjectId, dest: String) -> Outcome {
        let pool = Arc::clone(self);
        let mut state = pool.state.lock().await;
        let destination = Path::new(&dest);
        if !destination.is_absolute() || destination.starts_with(&state.root) {
            return Outcome::Error(ErrorBody::new(
                ErrorCode::InvalidArgument,
                "a pool destination must be absolute and outside the ready pool",
            ));
        }
        if state.size == 0 {
            return Outcome::Ok(Response::PoolTaken(None));
        }
        let Some(index) = pick(&state.ready, tree) else {
            let root = state.root.clone();
            drop(state);
            return super::template::take_clone(&pool.template, &root, tree, dest).await;
        };
        let slot = state.ready.remove(index);
        // Removing the slot under the same lock as refresh makes ownership
        // exclusive even when two clients ask for the same tree together.
        let result = publish_slot(&slot, dest, &state.root).await;
        let taken = PooledWorkspace {
            files: slot.files,
            prepared: slot.prep == Prep::Done,
            from_template: false,
        };
        if result.is_err() {
            state.ready.push(slot);
        }
        drop(state);
        pool.wake.notify_one();
        match result {
            Ok(()) => Outcome::Ok(Response::PoolTaken(Some(taken))),
            Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
                Outcome::Ok(Response::PoolTaken(None))
            }
            Err(error) => Outcome::Error(ErrorBody::new(
                ErrorCode::InvalidArgument,
                error.to_string(),
            )),
        }
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

async fn publish_slot(slot: &Slot, dest: String, pool_root: &Path) -> std::io::Result<()> {
    let source = slot.path.clone();
    let limits_root = pool_root
        .parent()
        .expect("pool data directory")
        .join("workspace-limits");
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
/// `tree`. Untracked files, such as prepared build output, are kept.
pub(super) fn refresh(
    store: &LooseObjectStore,
    slot: &mut Slot,
    tree: ObjectId,
) -> Result<(), MaterialiseError> {
    let before = Layout::of_tree(store, slot.tree, &Sparse::all())?;
    let after = Layout::of_tree(store, tree, &Sparse::all())?;
    let changed = super::bring_to_tree(&slot.path, &before, &after)?;
    super::write_all(store, &slot.path, &changed)?;
    slot.tree = tree;
    slot.files = after.files.len() as u64;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosen_core::{FileMode, Tree, TreeEntry};
    use rosen_store::ObjectStore;

    // R3 critical contract: two simultaneous takes must not give agents the
    // same directory, where either agent could overwrite the other's work.
    #[tokio::test]
    async fn concurrent_takes_transfer_a_refreshed_slot_once() {
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
        refresh(&store, &mut slot, after).unwrap();
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
