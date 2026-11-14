//! Trees and commits a sparse fill stored without all their blobs (SPEC D8,
//! R3; issue #67).
//!
//! The cache's store otherwise keeps the server's invariant: a stored tree or
//! commit has everything it reaches stored too. A sparse fill fetches every
//! tree but only the chosen blobs, so the trees it stores may lack blobs.
//! Each such object is recorded here before it is written. A fill walks a
//! recorded object rather than trusting it, and drops the record once it
//! finds the object's closure complete. Every stored tree, recorded or not,
//! still has all its subtrees stored.
//!
//! The set lives in memory, backed by an append-only log: `+<id>` records a
//! partial object and `-<id>` a completed one. Records are appended under
//! one lock, so the log's order is the set's. A `+` reaches the disk before
//! its object is written. A `-` need not: a stale `+` only costs a walk.

use rosen_core::{ObjectId, ObjectKind};
use rosen_store::{ObjectStore, RawObject, StoreError};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const LOG_NAME: &str = "partial-objects.log";

pub struct PartialObjects {
    state: Mutex<State>,
}

struct State {
    ids: HashSet<ObjectId>,
    log: File,
}

impl PartialObjects {
    /// Loads the set from `data_dir`, rewriting its log with only the live
    /// records so the log never grows without bound.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let path = data_dir.join(LOG_NAME);
        let ids = match std::fs::read_to_string(&path) {
            Ok(log) => replay(&log),
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashSet::new(),
            Err(err) => return Err(err),
        };
        let live: String = ids.iter().map(|id| format!("+{id}\n")).collect();
        atomicwrites::AtomicFile::new(&path, atomicwrites::AllowOverwrite)
            .write(|file| file.write_all(live.as_bytes()))
            .map_err(|err| match err {
                atomicwrites::Error::Internal(err) | atomicwrites::Error::User(err) => err,
            })?;
        let log = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            state: Mutex::new(State { ids, log }),
        })
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.lock().ids.contains(id)
    }

    /// Records `ids` as partial, durably: call before writing them.
    pub fn mark(&self, ids: &[ObjectId]) -> io::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut state = self.lock();
        append(&mut state.log, '+', ids)?;
        state.log.sync_data()?;
        state.ids.extend(ids.iter().copied());
        Ok(())
    }

    /// Drops the records of `ids`, whose closures are now complete. Only
    /// call once every object they reach is written.
    pub fn clear(&self, ids: &[ObjectId]) -> io::Result<()> {
        let mut state = self.lock();
        let recorded: Vec<ObjectId> = ids
            .iter()
            .filter(|id| state.ids.contains(id))
            .copied()
            .collect();
        if recorded.is_empty() {
            return Ok(());
        }
        // Forgotten first: should the append fail, the log still holds `+`,
        // which is the safe side.
        for id in &recorded {
            state.ids.remove(id);
        }
        append(&mut state.log, '-', &recorded)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Each batch starts on a fresh line, so a record torn by an earlier failed
/// write never runs into the next one.
fn append(log: &mut File, sign: char, ids: &[ObjectId]) -> io::Result<()> {
    let mut records = String::from("\n");
    for id in ids {
        records.push(sign);
        records.push_str(&id.to_hex());
        records.push('\n');
    }
    log.write_all(records.as_bytes())
}

/// Lines that do not parse were torn by a crash; their objects were never
/// written, because a `+` is durable before its object is.
fn replay(log: &str) -> HashSet<ObjectId> {
    let mut ids = HashSet::new();
    for line in log.lines() {
        let Some(sign) = line.chars().next() else {
            continue;
        };
        let Ok(id) = ObjectId::from_hex(&line[sign.len_utf8()..]) else {
            continue;
        };
        match sign {
            '+' => ids.insert(id),
            '-' => ids.remove(&id),
            _ => false,
        };
    }
    ids
}

/// The store as seen by a check that a tree's entries are all present
/// before keeping it: a partial object counts as absent, so a tree naming
/// one is never kept as complete.
pub struct CompleteView<'a> {
    pub store: &'a dyn ObjectStore,
    pub partial: &'a PartialObjects,
}

impl ObjectStore for CompleteView<'_> {
    fn put_raw(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId, StoreError> {
        self.store.put_raw(kind, data)
    }

    fn get_raw(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
        self.store.get_raw(id)
    }

    /// The store is asked first: a record is made before its object is
    /// written, so an object found stored has its record already visible.
    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
        Ok(self.store.contains(id)? && !self.partial.contains(id))
    }
}
