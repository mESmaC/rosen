//! Server-side file edits (SPEC R3), applied directly to a change head.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWrite {
    pub change: String,
    /// Repository-relative file path. Parent directories are created as needed.
    pub path: String,
    /// None deletes a file. Some writes arbitrary bytes, including empty files.
    pub data: Option<WorkspaceBytes>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBytes(#[serde(with = "crate::snapshot::base64_bytes")] pub Vec<u8>);
