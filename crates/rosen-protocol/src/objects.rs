//! Wire shapes for reading objects through a daemon (SPEC D8, §3 "Object
//! store"): how a machine's tier-1 cache fills its store from the server, and
//! how a client asks its daemon to make a tree readable from the local store.

use rosen_core::{ObjectId, ObjectKind};
use serde::{Deserialize, Serialize};

/// The kind of a stored object, as sent on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireKind {
    Blob,
    Tree,
    Commit,
}

impl From<ObjectKind> for WireKind {
    fn from(kind: ObjectKind) -> Self {
        match kind {
            ObjectKind::Blob => WireKind::Blob,
            ObjectKind::Tree => WireKind::Tree,
            ObjectKind::Commit => WireKind::Commit,
        }
    }
}

impl From<WireKind> for ObjectKind {
    fn from(kind: WireKind) -> Self {
        match kind {
            WireKind::Blob => ObjectKind::Blob,
            WireKind::Tree => ObjectKind::Tree,
            WireKind::Commit => ObjectKind::Commit,
        }
    }
}

/// One object body, without the `<kind> <len>\0` header. The receiver derives
/// its ID from the bytes, so a cache can never store an object under the
/// wrong ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireObject {
    pub kind: WireKind,
    #[serde(with = "crate::snapshot::base64_bytes")]
    pub data: Vec<u8>,
}

/// Parameters of [`crate::Request::GetObjects`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectQuery {
    pub ids: Vec<ObjectId>,
}

/// The answer to [`crate::Request::GetObjects`]. An asked ID that is in
/// neither list was deferred because the frame was full; ask for it again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectBatch {
    pub objects: Vec<WireObject>,
    /// IDs the store does not hold.
    pub missing: Vec<ObjectId>,
}

/// Parameters of [`crate::Request::EnsureObjects`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnsureObjects {
    /// A tree, commit or blob. For a commit, its tree is made local too, but
    /// not its parents.
    pub root: ObjectId,
    /// A sparse workspace's chosen paths, `/`-separated from the root tree
    /// (R3); empty for the whole tree. A cache then fetches every tree under
    /// `root` but only the blobs at or under these paths. The server's store
    /// is always complete, so the server ignores them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sparse: Vec<String>,
}

/// The answer to [`crate::Request::EnsureObjects`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectsLocal {
    /// Objects the daemon fetched from the server to answer; 0 on the server
    /// itself and on a cache that already held them all.
    pub fetched: u64,
}
