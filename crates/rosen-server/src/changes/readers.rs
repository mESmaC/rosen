//! Bounded read-only registry connections, separate from the writer mutex.
use super::ChangeError;
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError, TryLockError};

pub(super) struct Readers {
    path: Option<PathBuf>,
    slots: [Mutex<Option<Connection>>; 8],
    next: AtomicUsize,
}

impl Readers {
    pub(super) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            slots: std::array::from_fn(|_| Mutex::new(None)),
            next: AtomicUsize::new(0),
        }
    }

    pub(super) fn with_conn<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, ChangeError>,
    ) -> Result<T, ChangeError> {
        let start = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        // A round-robin slot may be busy while another is idle. Only wait
        // after checking the whole pool; no I/O holds a pool-wide mutex.
        let mut slot = (0..self.slots.len())
            .find_map(
                |offset| match self.slots[(start + offset) % self.slots.len()].try_lock() {
                    Ok(slot) => Some(slot),
                    Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
                    Err(TryLockError::WouldBlock) => None,
                },
            )
            .unwrap_or_else(|| {
                self.slots[start]
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            });
        if slot.is_none() {
            let path = self.path.as_ref().expect("file-backed registry reader");
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = Connection::open_with_flags(path, flags)?;
            crate::metadata::configure(&conn)?;
            *slot = Some(conn);
        }
        // A record includes its head, snapshots and conflicts in separate
        // queries. Keep them in one WAL snapshot while writers keep moving.
        let conn = slot.as_ref().expect("reader opened above");
        let tx = conn.unchecked_transaction()?;
        let result = work(&tx)?;
        tx.commit()?;
        Ok(result)
    }
}
