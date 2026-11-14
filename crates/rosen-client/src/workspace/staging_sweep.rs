//! Removes staging directories that crashed materialisations left behind.
//!
//! A workspace is built in `.<name>.rosen-partial-<pid>-<attempt>` beside its
//! destination, and only the building process removes it when the build does
//! not publish. A process killed mid-build leaves it there for good, holding
//! a partial tree and, on the clone path, the template's build output. The
//! name carries the builder's process id, so a directory whose builder no
//! longer runs is abandoned and nothing else can be writing to it.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The tag between a staging directory's destination name and its owner.
pub(super) const STAGING_TAG: &str = ".rosen-partial-";

/// How often one process looks for abandoned staging in the same directory.
/// The pool materialises at every land, and a crash is rare, so a listing per
/// build would be wasted work.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Starts removing the abandoned staging directories in `parent`, at most
/// once per [`SWEEP_INTERVAL`] for each `parent` in this process. Removal runs
/// on its own thread, since a partial clone can hold a whole build's output
/// and deleting it must not delay the workspace being built. A removal cut
/// short when the process exits resumes at the next sweep.
pub(super) fn sweep_abandoned(parent: &Path) {
    static LAST_SWEEP: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    let now = Instant::now();
    {
        let mut last = LAST_SWEEP
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last
            .get(parent)
            .is_some_and(|at| now.duration_since(*at) < SWEEP_INTERVAL)
        {
            return;
        }
        last.insert(parent.to_path_buf(), now);
    }
    let abandoned = find_abandoned(parent);
    if abandoned.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("rosen-staging-sweep".into())
        .spawn(move || {
            for dir in abandoned {
                // Best effort: whatever stays is retried at the next sweep.
                let _ = fs::remove_dir_all(dir);
            }
        });
}

/// The staging directories in `parent` whose building process no longer
/// runs. A link carrying a staging name is skipped, so removal never follows
/// it out of `parent`.
pub(super) fn find_abandoned(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let own = std::process::id();
    entries
        .flatten()
        .filter(|entry| {
            staging_owner(&entry.file_name()).is_some_and(|pid| {
                pid != own && !rosen_platform::process_may_be_running(pid)
            })
        })
        // `DirEntry::file_type` does not follow links, and a junction or
        // symlink is never a directory here.
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// Whether `name` has the staging form. Rosen refuses it for a workspace, so
/// a directory with this name beside workspaces is only ever staging and a
/// sweep never removes a published workspace.
pub(crate) fn is_staging_name(name: &OsStr) -> bool {
    staging_owner(name).is_some()
}

/// The process id in a staging directory's name, or `None` for any other
/// name. The tag and owner are ASCII, so a destination name that is not
/// valid Unicode still parses.
fn staging_owner(name: &OsStr) -> Option<u32> {
    let name = name.to_string_lossy();
    let rest = name.strip_prefix('.')?;
    let (dest_name, owner) = rest.rsplit_once(STAGING_TAG)?;
    let (pid, attempt) = owner.split_once('-')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if dest_name.is_empty() || !digits(pid) || !digits(attempt) {
        return None;
    }
    pid.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Risk: removing a live build's staging would make a concurrent
    // materialisation fail or publish a damaged workspace, so only a dead
    // builder's directory may be found, and never a workspace's.
    #[test]
    fn finds_only_dead_builders_staging() {
        let root = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let own = std::process::id();
        let dead_dir = root.path().join(format!(".ws{STAGING_TAG}{dead}-3"));
        for name in [
            format!(".ws{STAGING_TAG}{dead}-3"),
            format!(".ws{STAGING_TAG}{own}-0"),
            format!("ws{STAGING_TAG}{dead}-0"),
            format!(".ws{STAGING_TAG}{dead}-0x"),
            "ws".to_string(),
        ] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        assert_eq!(find_abandoned(root.path()), vec![dead_dir]);
    }
}
