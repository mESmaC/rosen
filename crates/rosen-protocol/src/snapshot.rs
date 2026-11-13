//! Wire shapes for write-through snapshots and `rosen commit` (SPEC R2, D17).
//!
//! A client uploads a snapshot in two steps. It first sends the objects the
//! server may lack, in [`crate::Request::PutObjects`] batches: the blobs of
//! files that changed since its last snapshot, then the trees on the paths
//! from those files up to the root. It then names the new root tree in
//! [`crate::Request::Snapshot`], and the server writes the snapshot commit.
//! Unchanged files and subtrees are never sent again.

use crate::change::ChangeRecord;
use rosen_core::ObjectId;
use serde::{Deserialize, Serialize};

/// The object kinds a client may upload. Commits are written only by the
/// server, so a client can never forge a change's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadKind {
    Blob,
    Tree,
}

/// One git-format object body, without the `<kind> <len>\0` header. The
/// server derives its ID from the bytes, so a client cannot mislabel one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadObject {
    pub kind: UploadKind,
    /// Base64, because frames are JSON and blobs are arbitrary bytes.
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

/// Parameters of [`crate::Request::PutObjects`]. A tree must come after (or
/// in an earlier batch than) every blob and subtree it names that the server
/// does not already hold: the server refuses a tree with a missing entry, so
/// a stored tree is always complete.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectUpload {
    pub objects: Vec<UploadObject>,
}

/// The answer to [`crate::Request::PutObjects`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectsStored {
    /// The IDs of the uploaded objects, in upload order.
    pub ids: Vec<ObjectId>,
}

/// Parameters of [`crate::Request::Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRequest {
    /// A change ID or unique prefix of one.
    pub change: String,
    /// The workspace's whole tree. The server must already hold it.
    pub tree: ObjectId,
    /// `Some` for `rosen commit`, which names the snapshot; `None` for an
    /// automatic snapshot from the watcher.
    pub message: Option<String>,
}

/// The answer to [`crate::Request::Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotOutcome {
    /// The change after the snapshot.
    pub change: ChangeRecord,
    /// The snapshot's commit. When `recorded` is false, the existing
    /// snapshot that already holds the tree.
    pub commit: Option<ObjectId>,
    /// False when an automatic snapshot named the tree the change's latest
    /// snapshot already holds, so nothing new was stored. A retried upload
    /// is therefore harmless.
    pub recorded: bool,
}

/// Serde adapter writing bytes as standard padded base64.
pub(crate) mod base64_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<Vec<u8>, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(input)?;
        STANDARD.decode(text.as_bytes()).map_err(D::Error::custom)
    }
}
