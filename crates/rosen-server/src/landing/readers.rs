//! WAL lookups must not wait on the ledger connection's receipt/CAS writer.
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags};

use super::{LandError, LandResult};

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

    pub(super) fn get<'a>(&'a self, writer: &'a Mutex<Connection>) -> LandResult<ReadGuard<'a>> {
        let Some(path) = &self.path else {
            // Separate connections would see different in-memory databases.
            return writer.lock().map(ReadGuard::Memory).map_err(poisoned);
        };
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let mut reader = self.slots[index].lock().map_err(poisoned)?;
        if reader.is_none() {
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = Connection::open_with_flags(path, flags)?;
            crate::metadata::configure(&conn)?;
            *reader = Some(conn);
        }
        Ok(ReadGuard::File(reader))
    }
}

pub(super) enum ReadGuard<'a> {
    Memory(MutexGuard<'a, Connection>),
    File(MutexGuard<'a, Option<Connection>>),
}

impl Deref for ReadGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        match self {
            Self::Memory(conn) => conn,
            Self::File(conn) => conn.as_ref().expect("reader opened before guard creation"),
        }
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> LandError {
    LandError::Store("ledger connection mutex poisoned".into())
}
