//! Losing live attempts can discard scratch objects instead of flushing them.
use super::{HistoryResult, ObjectId, ObjectKind};
use rosen_store::{ObjectStore, RawObject, StoreError};
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

const SCRATCH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct Pending {
    objects: HashMap<ObjectId, RawObject>,
    bytes: usize,
}

pub(super) struct Scratch<'a> {
    backing: &'a dyn ObjectStore,
    pending: Mutex<Pending>,
}

impl<'a> Scratch<'a> {
    pub(super) fn new(backing: &'a dyn ObjectStore) -> Self {
        Self {
            backing,
            pending: Mutex::default(),
        }
    }

    /// No returned merge tree or cache entry may name scratch-only objects.
    pub(super) fn persist(self, current: &dyn Fn() -> HistoryResult<bool>) -> HistoryResult<bool> {
        let pending = self
            .pending
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        for object in pending.objects.into_values() {
            if !current()? {
                return Ok(false);
            }
            self.backing.put_raw(object.kind, &object.data)?;
        }
        current()
    }
}

impl ObjectStore for Scratch<'_> {
    fn put_raw(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId, StoreError> {
        let id = ObjectId::hash_object(kind.as_str(), data);
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.objects.contains_key(&id) {
            return Ok(id);
        }
        // Large merges spill to the durable store rather than retaining an
        // unbounded amount of speculative data for every concurrent request.
        let bytes = data.len().saturating_add(256);
        if bytes > SCRATCH_BYTES - pending.bytes {
            drop(pending);
            return self.backing.put_raw(kind, data);
        }
        pending.objects.insert(
            id,
            RawObject {
                kind,
                data: data.to_vec(),
            },
        );
        pending.bytes += bytes;
        Ok(id)
    }

    fn get_raw(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
        let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(object) = pending.objects.get(id) {
            return Ok(Some(object.clone()));
        }
        drop(pending);
        self.backing.get_raw(id)
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
        if self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .objects
            .contains_key(id)
        {
            return Ok(true);
        }
        self.backing.contains(id)
    }

    // The default reads the whole object; the backing store reads only its
    // header, which keeps sizing a large blob cheap (#397).
    fn object_size(&self, id: &ObjectId) -> Result<u64, StoreError> {
        let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(object) = pending.objects.get(id) {
            return Ok(object.data.len() as u64);
        }
        drop(pending);
        self.backing.object_size(id)
    }
}
