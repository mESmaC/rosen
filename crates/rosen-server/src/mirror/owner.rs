//! Which server owns the mirror and Collapse feeds (SPEC R1, #401, #433).
//!
//! Every server for a project reads the same `[mirror]` and `collapse`
//! settings, so a second setup, another machine's server or a server an
//! agent starts for a test would all push to one remote and send colliding
//! Collapse sequences. Only the server marked as the mirror owner pushes
//! the mirror, sends the Collapse state feed and ledger entries, and runs
//! the Collapse action relay; the others keep the settings and send nothing.
//!
//! The mark is a file in the data directory holding that directory's own
//! canonical path. A copied data directory carries the file (and any relay
//! allow-list) but not the path, so a copy of the project never pushes,
//! sends to Collapse or runs relayed actions until someone marks it.

use std::path::{Path, PathBuf};

/// The mark's file name in the data directory.
const MARK_FILE: &str = "mirror-owner";

/// Names a data directory whose server owns the mirror, for a daemon that
/// a client starts with no flags. Its value is the data directory, not a
/// switch, so a test server inheriting it from an agent's shell does not
/// become an owner.
pub const OWNER_ENV: &str = "ROSEN_MIRROR_OWNER";

/// Marks the server at `data_dir` as the mirror owner, surviving restarts.
pub fn mark(data_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let own = canonical(data_dir)?;
    std::fs::write(data_dir.join(MARK_FILE), own.to_string_lossy().as_bytes())
}

/// Removes the mark, so the server at `data_dir` stops pushing.
pub fn unmark(data_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(data_dir.join(MARK_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// True when the mark names `data_dir` itself. Any read failure counts as
/// not the owner: a server that cannot tell must not push.
pub fn is_owner(data_dir: &Path) -> bool {
    let Ok(marked) = std::fs::read_to_string(data_dir.join(MARK_FILE)) else {
        return false;
    };
    canonical(data_dir).is_ok_and(|own| same_dir(Path::new(marked.trim()), &own))
}

/// Runs [`is_owner`] on the blocking pool so background tasks do not touch
/// the file system on an async worker. Fails closed (`false`) if the task
/// cannot complete.
pub async fn is_owner_async(data_dir: PathBuf) -> bool {
    tokio::task::spawn_blocking(move || is_owner(&data_dir))
        .await
        .unwrap_or(false)
}

/// True when the `ROSEN_MIRROR_OWNER` value `named` is `data_dir`.
pub fn names(named: &Path, data_dir: &Path) -> bool {
    match (canonical(named), canonical(data_dir)) {
        (Ok(named), Ok(own)) => same_dir(&named, &own),
        _ => false,
    }
}

fn canonical(dir: &Path) -> std::io::Result<PathBuf> {
    Ok(without_verbatim_prefix(std::fs::canonicalize(dir)?))
}

/// Strips Windows' `\\?\` prefix from a plain drive path, so the stored
/// path reads as the operator wrote it. Comparison never depends on it:
/// both sides go through this function.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// Windows paths compare without case, as the file system does.
fn same_dir(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}
