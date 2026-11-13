//! Copies objects from a git source into an object store.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Mutex;

use rosen_core::{ObjectId, ObjectKind};
use rosen_store::ObjectStore;

use super::{GitSource, ImportError};

/// Objects queued per write worker. Bounds memory to a few objects per
/// worker while git reads ahead.
const QUEUE_DEPTH_PER_WORKER: usize = 4;

/// One object read from git and waiting to be stored.
struct PendingObject {
    id: ObjectId,
    kind: ObjectKind,
    payload: Vec<u8>,
}

/// The ids in `ids` that `store` does not hold yet. Lets a rerun after a
/// crash skip what the earlier run stored.
pub fn filter_missing(
    ids: &[ObjectId],
    store: &dyn ObjectStore,
) -> Result<Vec<ObjectId>, ImportError> {
    let mut missing = Vec::new();
    for id in ids {
        if !store.contains(id).map_err(store_error)? {
            missing.push(*id);
        }
    }
    Ok(missing)
}

/// Reads `ids` from `source` and stores each in `store`, checking that the
/// store hashes every object to the id git gave it.
///
/// Git reads objects on one thread; compressing and writing loose objects is
/// the slower part, so that runs on one worker per core.
pub fn copy_objects(
    source: &GitSource,
    ids: &[ObjectId],
    store: &dyn ObjectStore,
) -> Result<(), ImportError> {
    if ids.is_empty() {
        return Ok(());
    }
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    let (sender, receiver) = sync_channel::<PendingObject>(workers * QUEUE_DEPTH_PER_WORKER);
    let receiver = Mutex::new(receiver);
    let failure = Mutex::new(None);
    let failed = AtomicBool::new(false);
    let read_result = std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| store_until_done(&receiver, store, &failure, &failed));
        }
        let mut enqueue = |id: ObjectId, kind: ObjectKind, payload: Vec<u8>| {
            // A worker failure stops reading early; the failure is reported
            // below. A send fails only once every worker has stopped.
            if failed.load(Ordering::Relaxed) {
                return Err(ImportError::Store("an object write failed".into()));
            }
            let pending = PendingObject { id, kind, payload };
            sender
                .send(pending)
                .map_err(|_| ImportError::Store("object writers stopped".into()))
        };
        let result = source.read_objects(ids, &mut enqueue);
        drop(sender);
        result
    });
    match failure
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        Some(error) => Err(error),
        None => read_result,
    }
}

fn store_until_done(
    receiver: &Mutex<Receiver<PendingObject>>,
    store: &dyn ObjectStore,
    failure: &Mutex<Option<ImportError>>,
    failed: &AtomicBool,
) {
    loop {
        let next = receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv();
        let Ok(pending) = next else { return };
        if failed.load(Ordering::Relaxed) {
            continue; // Drain the queue so the reader never blocks on a full one.
        }
        if let Err(error) = store_one(store, pending) {
            failed.store(true, Ordering::Relaxed);
            failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_or_insert(error);
        }
    }
}

fn store_one(store: &dyn ObjectStore, pending: PendingObject) -> Result<(), ImportError> {
    let stored = store
        .put_raw(pending.kind, &pending.payload)
        .map_err(store_error)?;
    if stored != pending.id {
        return Err(ImportError::IdMismatch {
            expected: pending.id,
            stored,
        });
    }
    Ok(())
}

fn store_error(error: rosen_store::StoreError) -> ImportError {
    ImportError::Store(error.to_string())
}
