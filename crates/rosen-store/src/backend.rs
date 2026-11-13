//! The pluggable storage backend interface (SPEC D5) and an in-memory backend.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::RwLock;

use rosen_core::{Blob, Commit, Object, ObjectId, ObjectKind, Tree};

/// An object's kind and its git-format body, without the `<kind> <len>\0` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawObject {
    pub kind: ObjectKind,
    pub data: Vec<u8>,
}

impl RawObject {
    pub fn id(&self) -> ObjectId {
        ObjectId::hash_object(self.kind.as_str(), &self.data)
    }

    pub fn decode(&self) -> rosen_core::Result<Object> {
        Object::decode_body(self.kind, &self.data)
    }
}

/// A content-addressed store of immutable git objects.
///
/// Backends implement the three raw operations; the typed helpers are shared.
/// Writes are idempotent: putting an object that already exists is a no-op that
/// returns the same ID. v1 never deletes objects (SPEC §1, garbage collection).
pub trait ObjectStore: Send + Sync {
    /// Stores an object durably and returns its git object ID. Once this
    /// returns, the object survives a crash.
    fn put_raw(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId, StoreError>;

    /// Reads an object, or `None` if the store does not hold it.
    fn get_raw(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError>;

    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError>;

    /// The length of an object's body, for sizing work before reading it.
    /// It is not checked against the object's hash; a later read is.
    fn object_size(&self, id: &ObjectId) -> Result<u64, StoreError> {
        let raw = self.get_raw(id)?.ok_or(StoreError::NotFound(*id))?;
        Ok(raw.data.len() as u64)
    }

    fn put(&self, object: &Object) -> Result<ObjectId, StoreError> {
        self.put_raw(object.kind(), &object.encode_body())
    }

    fn put_blob(&self, data: &[u8]) -> Result<ObjectId, StoreError> {
        self.put_raw(ObjectKind::Blob, data)
    }

    fn put_tree(&self, tree: &Tree) -> Result<ObjectId, StoreError> {
        self.put(&Object::Tree(tree.clone()))
    }

    fn put_commit(&self, commit: &Commit) -> Result<ObjectId, StoreError> {
        self.put(&Object::Commit(Box::new(commit.clone())))
    }

    /// Reads and decodes an object that must exist.
    fn read(&self, id: &ObjectId) -> Result<Object, StoreError> {
        let raw = self.get_raw(id)?.ok_or(StoreError::NotFound(*id))?;
        raw.decode()
            .map_err(|error| StoreError::Malformed(*id, error))
    }

    fn read_blob(&self, id: &ObjectId) -> Result<Blob, StoreError> {
        match self.read(id)? {
            Object::Blob(blob) => Ok(blob),
            other => Err(wrong_kind(id, ObjectKind::Blob, &other)),
        }
    }

    fn read_tree(&self, id: &ObjectId) -> Result<Tree, StoreError> {
        match self.read(id)? {
            Object::Tree(tree) => Ok(tree),
            other => Err(wrong_kind(id, ObjectKind::Tree, &other)),
        }
    }

    fn read_commit(&self, id: &ObjectId) -> Result<Commit, StoreError> {
        match self.read(id)? {
            Object::Commit(commit) => Ok(*commit),
            other => Err(wrong_kind(id, ObjectKind::Commit, &other)),
        }
    }
}

fn wrong_kind(id: &ObjectId, expected: ObjectKind, found: &Object) -> StoreError {
    StoreError::WrongKind {
        id: *id,
        expected,
        found: found.kind(),
    }
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    NotFound(ObjectId),
    WrongKind {
        id: ObjectId,
        expected: ObjectKind,
        found: ObjectKind,
    },
    /// Stored bytes are damaged: undecodable, or not hashing to their ID.
    Corrupt {
        id: ObjectId,
        reason: String,
    },
    /// The object is intact but its body is not valid git format for its kind.
    Malformed(ObjectId, rosen_core::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "object store I/O error: {error}"),
            Self::NotFound(id) => write!(f, "object {id} not found"),
            Self::WrongKind {
                id,
                expected,
                found,
            } => write!(
                f,
                "object {id} is a {}, expected a {}",
                found.as_str(),
                expected.as_str()
            ),
            Self::Corrupt { id, reason } => write!(f, "object {id} is corrupt: {reason}"),
            Self::Malformed(id, error) => write!(f, "object {id}: {error}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Malformed(_, error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A non-durable store for tests and scratch computations, such as a merge
/// result built before anything is written for real.
#[derive(Debug, Default)]
pub struct MemoryObjectStore {
    objects: RwLock<HashMap<ObjectId, RawObject>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.objects.read().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ObjectStore for MemoryObjectStore {
    fn put_raw(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId, StoreError> {
        let id = ObjectId::hash_object(kind.as_str(), data);
        let mut objects = self.objects.write().unwrap_or_else(|p| p.into_inner());
        objects.entry(id).or_insert_with(|| RawObject {
            kind,
            data: data.to_vec(),
        });
        Ok(id)
    }

    fn get_raw(&self, id: &ObjectId) -> Result<Option<RawObject>, StoreError> {
        let objects = self.objects.read().unwrap_or_else(|p| p.into_inner());
        Ok(objects.get(id).cloned())
    }

    fn contains(&self, id: &ObjectId) -> Result<bool, StoreError> {
        let objects = self.objects.read().unwrap_or_else(|p| p.into_inner());
        Ok(objects.contains_key(id))
    }
}
