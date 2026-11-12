//! Runs per-file jobs on a pool of threads. A workspace's files are many and
//! small, so a single thread spends most of its time waiting on file creation;
//! several threads keep the disk busy (SPEC.md R3, parallel plain copy).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

/// More threads than this only queue up behind the file system.
const MAX_THREADS: usize = 16;

/// Below this many jobs per thread, starting threads costs more than it saves.
const JOBS_PER_THREAD: usize = 32;

/// Runs `work` on every job, stopping early at the first error and returning it.
pub fn for_each<T, E, F>(jobs: &[T], work: F) -> Result<(), E>
where
    T: Sync,
    E: Send,
    F: Fn(&T) -> Result<(), E> + Sync,
{
    let threads = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_THREADS)
        .min(jobs.len().div_ceil(JOBS_PER_THREAD))
        .max(1);
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error = Mutex::new(None);
    let worker = || {
        while !failed.load(Ordering::Relaxed) {
            let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) else {
                return;
            };
            if let Err(error) = work(job) {
                failed.store(true, Ordering::Relaxed);
                first_error
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get_or_insert(error);
                return;
            }
        }
    };
    thread::scope(|scope| {
        for _ in 1..threads {
            scope.spawn(worker);
        }
        worker();
    });
    match first_error.into_inner().unwrap_or_else(|p| p.into_inner()) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
