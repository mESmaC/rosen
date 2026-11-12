//! Machine-local ready workspaces (SPEC R3). The daemon owns this manager.
use super::{layout::Layout, MaterialiseError, Sparse};
use rosen_core::ObjectId;
use rosen_protocol::{ErrorBody, ErrorCode, Outcome, Response};
use rosen_store::LooseObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

pub struct WarmPool {
    pub(super) state: Mutex<State>,
    pub(super) wake: Notify,
}

pub(super) struct State {
    pub root: PathBuf,
    pub size: usize,
    pub next: u64,
    pub ready: Vec<Slot>,
}

pub(super) struct Slot {
    pub path: PathBuf,
    pub tree: ObjectId,
    pub files: u64,
}

impl WarmPool {
    pub fn new(data_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                root: data_dir.join("warm-pool"),
                size: 4,
                next: 0,
                ready: Vec::new(),
            }),
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
        self.state.lock().await.size = size;
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
        let Some(index) = state.ready.iter().position(|slot| slot.tree == tree) else {
            return Outcome::Ok(Response::PoolTaken(None));
        };
        let slot = state.ready.remove(index);
        // Removing the slot under the same lock as refresh makes ownership
        // exclusive even when two clients ask for the same tree together.
        let source = slot.path.clone();
        let result = tokio::task::spawn_blocking(move || publish(&source, Path::new(&dest)))
            .await
            .unwrap_or_else(|error| Err(std::io::Error::other(error.to_string())));
        let files = slot.files;
        if result.is_err() {
            state.ready.push(slot);
        }
        drop(state);
        pool.wake.notify_one();
        match result {
            Ok(()) => Outcome::Ok(Response::PoolTaken(Some(files))),
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

fn publish(source: &Path, dest: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(dest).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "workspace destination exists",
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(source, dest)
}

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
            files: 1,
        };
        refresh(&store, &mut slot, after).unwrap();
        pool.state.lock().await.ready.push(slot);
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let (a, b) = tokio::join!(
            pool.take(after, first.display().to_string()),
            pool.take(after, second.display().to_string())
        );
        let transferred = |result| matches!(result, Outcome::Ok(Response::PoolTaken(Some(1))));
        assert_ne!(transferred(a), transferred(b));
        let taken = if first.exists() { first } else { second };
        assert_eq!(std::fs::read(taken.join("file")).unwrap(), b"after");
    }
}
