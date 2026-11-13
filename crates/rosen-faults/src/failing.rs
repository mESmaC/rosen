//! The injected failing commit (SPEC.md §5, R12).
//!
//! One agent's change adds a file named [`FILE_NAME`]. Its pre-land checks
//! pass, as a real regression's often do, but the post-land check
//! [`fails`] fails every landed tree that holds the file. That makes the
//! culprit exactly one land, so the run can check that bisection blamed it,
//! that the server reverted it, and that its change reopened with its work.

use anyhow::Context;
use rosen_core::{ObjectId, Tree};
use rosen_store::{LooseObjectStore, ObjectStore};

/// The file whose presence fails post-land checks.
pub const FILE_NAME: &str = "FAULT-INJECTED-FAILURE.rs";

pub fn contents(change: &str) -> String {
    format!("// injected post-land failure from change {change}\n")
}

/// The post-land check a stress checker runs on a landed tree: it fails
/// when the injected file is anywhere in it.
pub fn fails(store: &LooseObjectStore, tree: &ObjectId) -> anyhow::Result<bool> {
    let tree: Tree = store.read_tree(tree).context("reading a landed tree")?;
    for entry in tree.entries() {
        if entry.name == FILE_NAME.as_bytes() {
            return Ok(true);
        }
        if entry.mode.is_tree() && fails(store, &entry.id)? {
            return Ok(true);
        }
    }
    Ok(false)
}
