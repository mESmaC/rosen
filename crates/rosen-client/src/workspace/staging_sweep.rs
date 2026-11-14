//! Removes staging directories that crashed materialisations left behind.
//!
//! A workspace is built in `.<name>.rosen-partial-<pid>-<attempt>` beside its
//! destination, and only the building process removes it when the build does
//! not publish. A process killed mid-build leaves it there for good, holding
//! a partial tree and, on the clone path, the template's build output.
//!
//! The builder writes a marker file, the directory's name plus
//! [`MARKER_SUFFIX`], right after creating the directory, and removes it once
//! the directory is published or removed. The marker is the evidence a
//! directory is unpublished staging: a sweep removes only a marked directory
//! whose builder, named by the process id in the name, no longer runs. A
//! directory that merely has a staging name, such as one made before markers
//! existed, is never touched.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::limits::Budget;

/// The tag between a staging directory's destination name and its owner.
pub(super) const STAGING_TAG: &str = ".rosen-partial-";

/// Appended to a staging directory's name to name its marker file.
const MARKER_SUFFIX: &str = ".rosen-staging";

/// How often one process looks for abandoned staging in the same directory.
/// The pool materialises at every land, and a crash is rare, so a listing per
/// build would be wasted work.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The marker file that shows `dir` is unpublished staging.
fn marker_of(dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_owned();
    name.push(MARKER_SUFFIX);
    PathBuf::from(name)
}

/// Creates the staging directory `dir`, then its marker. Returns false when
/// `dir` already exists, so the caller tries another name. A crash between
/// the two leaves an unmarked directory, which is only ever leaked, never a
/// wrong deletion.
pub(super) fn create_marked(dir: &Path) -> io::Result<bool> {
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error),
    }
    // Created exclusively: a file already holding the marker's name, such
    // as one a builder with the same id left by crashing after publishing,
    // is never overwritten, and this name is given up for the next one.
    let created = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker_of(dir));
    match created {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_dir(dir);
            Ok(false)
        }
        // Any other failure, such as a name too long for its suffix, leaves
        // the build unmarked: it still runs, and a crash only leaks it.
        _ => Ok(true),
    }
}

/// Called once `dir` is renamed into place: the published workspace must
/// carry no evidence that would let a sweep take it for staging.
pub(super) fn unmark(dir: &Path) {
    let _ = fs::remove_file(marker_of(dir));
}

/// Removes unpublished staging `dir` and then its marker. The marker stays
/// while any of `dir` does, so a later sweep finishes the removal.
pub(super) fn remove_marked(dir: &Path) {
    if fs::remove_dir_all(dir).is_ok() {
        unmark(dir);
    }
}

/// Starts removing the abandoned staging directories in `parent`, at most
/// once per [`SWEEP_INTERVAL`] for each `parent` in this process. Removal runs
/// on its own thread, since a partial clone can hold a whole build's output
/// and deleting it must not delay the workspace being built. A removal cut
/// short when the process exits resumes at the next sweep.
pub(super) fn sweep_abandoned(parent: &Path, budget: &Budget) {
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
    let budget = budget.clone();
    let _ = std::thread::Builder::new()
        .name("rosen-staging-sweep".into())
        .spawn(move || {
            for marker in &abandoned {
                remove_abandoned(marker, &budget);
            }
        });
}

/// The markers in `parent` whose building process no longer runs. A marker
/// is always empty, so a file with contents is never taken for one and never
/// removed. A marker whose directory is gone, left by a crash just after
/// publishing, is listed too, so it is cleared.
fn find_abandoned(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let own = std::process::id();
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| entry.metadata().is_ok_and(|meta| meta.len() == 0))
        .filter_map(|entry| {
            let marker = entry.path();
            // The staging name ends in `-<attempt>`, with no dot, so the
            // marker's only extension is the suffix and its stem is the
            // directory's name, whatever encoding that name has.
            if marker.extension()? != OsStr::new(&MARKER_SUFFIX[1..]) {
                return None;
            }
            let pid = staging_owner(marker.file_stem()?)?;
            let abandoned = pid != own && !rosen_platform::process_may_be_running(pid);
            abandoned.then_some(marker)
        })
        .collect()
}

/// Removes the staging directory `marker` marks, then the marker, when its
/// builder still does not run. The listing that found `marker` may be stale:
/// another sweep may have removed that directory since, and a new builder
/// with the same, reused id may have made a new one under the name. So the
/// directory is first moved, atomically, to a tombstone name only this sweep
/// uses, and the builder is checked after the move. Whoever made the moved
/// directory had that id and was running when it made it, so if no process
/// has the id now, the moved directory is abandoned whatever the listing
/// said; if one does, it goes back. Each move goes through `budget`, so the
/// directory's bytes stay charged until they are deleted. Removal never
/// follows a link.
fn remove_abandoned(marker: &Path, budget: &Budget) {
    let Some(pid) = marker.file_stem().and_then(staging_owner) else {
        return;
    };
    let dir = marker.with_extension("");
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        // A marker whose directory was published or removed. Removing it
        // can at worst unmark a newer build, which only leaks that build.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !rosen_platform::process_may_be_running(pid) {
                let _ = fs::remove_file(marker);
            }
            return;
        }
        _ => return,
    }
    let Some(tomb) = mark_tombstone(&dir) else {
        return;
    };
    if budget.rename_staging(&dir, &tomb).is_err() {
        unmark(&tomb);
        return;
    }
    if rosen_platform::process_may_be_running(pid) {
        if budget.rename_staging(&tomb, &dir).is_ok() {
            unmark(&tomb);
        }
        return;
    }
    unmark(&dir);
    remove_marked(&tomb);
}

/// The destination name of every tombstone. It has the staging form, which
/// is reserved, so no builder ever stages under it; and it is short, so a
/// tombstone name fits wherever the staging name it replaces did.
const TOMBSTONE_DEST: &str = ".sweep.rosen-partial-0-0";

/// Creates the marker of a fresh tombstone name beside `dir`, owned by this
/// process. If this process dies mid-removal, the marked tombstone is swept
/// like any staging.
fn mark_tombstone(dir: &Path) -> Option<PathBuf> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let own = std::process::id();
    for _ in 0..8 {
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let tomb = dir.with_file_name(format!(".{TOMBSTONE_DEST}{STAGING_TAG}{own}-{serial}"));
        if fs::symlink_metadata(&tomb).is_ok() {
            continue;
        }
        let created = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker_of(&tomb));
        match created {
            Ok(_) => return Some(tomb),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Whether `name` has the form of a staging directory or its marker. Rosen
/// refuses it for a workspace, so it never publishes one a sweep could
/// mistake for staging.
pub(crate) fn is_staging_name(name: &OsStr) -> bool {
    let path = Path::new(name);
    staging_owner(name).is_some()
        || (path.extension() == Some(OsStr::new(&MARKER_SUFFIX[1..]))
            && path.file_stem().and_then(staging_owner).is_some())
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
    // materialisation fail or publish a damaged workspace, and removing an
    // unmarked directory could delete a workspace or user data. Only a dead
    // builder's marked directory may be found.
    #[test]
    fn finds_only_dead_builders_marked_staging() {
        let root = tempfile::tempdir().unwrap();
        let limits = tempfile::tempdir().unwrap();
        let budget = Budget::open(limits.path(), Default::default()).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let own = std::process::id();
        let staging = |name: String| root.path().join(name);
        let dead_marked = staging(format!(".ws{STAGING_TAG}{dead}-3"));
        assert!(create_marked(&dead_marked).unwrap());
        assert!(create_marked(&staging(format!(".ws{STAGING_TAG}{own}-0"))).unwrap());
        fs::create_dir(staging(format!(".ws{STAGING_TAG}{dead}-4"))).unwrap();
        fs::create_dir(staging(format!(".ws{STAGING_TAG}{dead}-0x"))).unwrap();
        fs::write(marker_of(&staging(format!(".ws{STAGING_TAG}{dead}-0x"))), b"").unwrap();
        let not_marker = staging(format!(".ws{STAGING_TAG}{dead}-5"));
        fs::create_dir(&not_marker).unwrap();
        fs::write(marker_of(&not_marker), b"user data").unwrap();
        assert_eq!(find_abandoned(root.path()), vec![marker_of(&dead_marked)]);
        // A file already holding a marker's name is kept and the name given up.
        let taken = staging(format!(".ws{STAGING_TAG}{own}-6"));
        fs::write(marker_of(&taken), b"user data").unwrap();
        assert!(!create_marked(&taken).unwrap() && !taken.exists());
        assert_eq!(fs::read(marker_of(&taken)).unwrap(), b"user data");
        remove_abandoned(&marker_of(&dead_marked), &budget);
        assert!(!dead_marked.exists() && !marker_of(&dead_marked).exists());
        let mut tombstones = fs::read_dir(root.path()).unwrap().flatten();
        assert!(tombstones.all(|entry| !entry.file_name().to_string_lossy().starts_with("..")));
        // A listing gone stale: the name now holds a running builder's
        // staging, which the claim and second check must leave intact.
        let live = staging(format!(".ws{STAGING_TAG}{own}-0"));
        let before = fs::read_dir(root.path()).unwrap().count();
        remove_abandoned(&marker_of(&live), &budget);
        assert!(live.is_dir() && marker_of(&live).is_file());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), before);
    }
}
