//! Server-side file edits (SPEC R3), applied directly to a change head.
use rosen_core::ObjectId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWrite {
    pub change: String,
    /// Repository-relative file path. Parent directories are created as needed.
    pub path: String,
    /// None deletes a file. Some writes arbitrary bytes, including empty files.
    pub data: Option<WorkspaceBytes>,
}

/// Parameters of [`crate::Request::WorkspaceWriteBlob`]: a write whose
/// bytes are too large for one frame (#243), so the client stores them
/// first with `PutBlobChunk` and names the blob. A request of its own, not
/// a field of [`WorkspaceWrite`], since a server that predates it would read
/// a write without `data` as a delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBlobWrite {
    pub change: String,
    /// Repository-relative file path. Parent directories are created as needed.
    pub path: String,
    /// A blob the server already holds.
    pub blob: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBytes(#[serde(with = "crate::snapshot::base64_bytes")] pub Vec<u8>);
