//! Messages for the machine-local warm pool (SPEC R3).

use rosen_core::ObjectId;
use serde::{Deserialize, Serialize};

/// A workspace the daemon handed over for a `pool_take`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PooledWorkspace {
    /// Tracked files the workspace holds.
    pub files: u64,
    /// The project's prepare commands all succeeded in it at its tree, so
    /// an agent's first build compiles only what it changes.
    pub prepared: bool,
    /// Block cloned from the daemon's template workspace because no ready
    /// workspace matched. False for a ready workspace renamed into place,
    /// or a template clone the file system refused and the daemon copied.
    pub from_template: bool,
}

/// Parameters of [`crate::Request::LandedProse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandedProseQuery {
    /// A base commit a workspace was prepared at.
    pub since: ObjectId,
    /// A later base commit.
    pub through: ObjectId,
}
