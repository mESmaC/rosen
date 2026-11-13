//! Messages for reading the project configuration (SPEC "Project
//! configuration").

use rosen_core::{ObjectId, ProjectConfig};
use serde::{Deserialize, Serialize};

/// Which commit's configuration to read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigQuery {
    /// A commit in the store; `None` reads the current base.
    #[serde(default)]
    pub at: Option<ObjectId>,
}

/// The configuration in force at one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRecord {
    /// The commit read.
    pub commit: ObjectId,
    /// The base ref's name when `commit` is the current base.
    pub base_ref: Option<String>,
    /// The commit whose `rosen.toml` is in force: `commit` itself, or its
    /// nearest first-parent ancestor with a valid file when `commit`'s is
    /// invalid. `None` when no such file exists and the defaults apply.
    pub source: Option<ObjectId>,
    /// Why `commit`'s own file was not accepted, if it was not.
    pub error: Option<String>,
    pub config: ProjectConfig,
}
