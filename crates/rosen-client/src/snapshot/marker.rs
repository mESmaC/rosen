//! The marker that makes a directory a Rosen workspace: `.rosen/workspace.json`
//! at its root, naming the change it belongs to and the last tree stored for
//! it. `rosen commit` finds its workspace by walking up to this file.

use rosen_core::{ChangeId, ObjectId};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The workspace's own state directory. Snapshots never include it.
pub const STATE_DIR: &str = ".rosen";
const MARKER_FILE: &str = "workspace.json";
/// Schema tag of the marker file. Bump on any shape change.
const MARKER_SCHEMA: &str = "rosen.workspace/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMarker {
    pub schema: String,
    pub change: ChangeId,
    /// The tree the server last stored for this workspace: the tree it was
    /// materialised at, then each snapshot's. A watcher that starts later
    /// compares the files on disk against it.
    pub tree: ObjectId,
    /// The sparse paths the workspace holds; empty for the whole tree. A
    /// tracked file outside them is absent from disk but not deleted.
    #[serde(default)]
    pub sparse: Vec<String>,
    /// The project's ignore list from the base's configuration (SPEC R2),
    /// kept here so a watcher started offline skips the same paths.
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl WorkspaceMarker {
    pub fn new(change: ChangeId, tree: ObjectId, sparse: Vec<String>, ignore: Vec<String>) -> Self {
        Self {
            schema: MARKER_SCHEMA.to_owned(),
            change,
            tree,
            sparse,
            ignore,
        }
    }

    pub fn path(root: &Path) -> PathBuf {
        root.join(STATE_DIR).join(MARKER_FILE)
    }

    pub fn read(root: &Path) -> io::Result<Self> {
        let text = fs::read(Self::path(root))?;
        serde_json::from_slice(&text).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    /// Writes through a temporary file and a rename, so a crash leaves the
    /// old marker or the new one, never a torn one.
    pub fn write(&self, root: &Path) -> io::Result<()> {
        let dir = root.join(STATE_DIR);
        fs::create_dir_all(&dir)?;
        let temp = dir.join(format!("{MARKER_FILE}.{}.tmp", std::process::id()));
        let body = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        fs::write(&temp, body)?;
        fs::rename(&temp, Self::path(root))
    }

    /// The nearest directory at or above `start` holding a marker.
    pub fn find_root(start: &Path) -> Option<PathBuf> {
        let start = std::path::absolute(start).ok()?;
        start
            .ancestors()
            .find(|dir| Self::path(dir).is_file())
            .map(Path::to_path_buf)
    }
}
