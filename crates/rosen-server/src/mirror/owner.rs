//! Which server owns the mirror and Collapse feeds (SPEC R1, #401, #433, #486).
//!
//! Every server for a project reads the same `[mirror]` and `collapse`
//! settings, so a second setup, another machine's server or a server an
//! agent starts for a test would all push to one remote and send colliding
//! Collapse sequences. Only the server marked as the mirror owner pushes
//! the mirror, sends the Collapse state feed and ledger entries, and runs
//! the Collapse action relay; the others keep the settings and send nothing.
//!
//! The mark is a file in the data directory bound to that directory's own
//! canonical path, the filesystem object identities of both the directory and
//! the mark file itself, and the OS machine identity when available. A copied
//! data directory carries the file (and any relay allow-list) but not those
//! identities, so a copy never pushes, sends to Collapse or runs relayed
//! actions — even at the same canonical path — until someone marks it.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use rosen_platform::file_id;

/// The mark's file name in the data directory.
const MARK_FILE: &str = "mirror-owner";

/// Current mark schema version. Plain-path marks from older versions are
/// rejected so a copy cannot inherit ownership on a same-path collision (#486).
const MARK_VERSION: u64 = 2;

/// Upper bound on a valid mark file so malformed input cannot allocate freely.
const MAX_MARK_BYTES: u64 = 16 * 1024;

/// Names a data directory whose server owns the mirror, for a daemon that
/// a client starts with no flags. Its value is the data directory, not a
/// switch, so a test server inheriting it from an agent's shell does not
/// become an owner.
pub const OWNER_ENV: &str = "ROSEN_MIRROR_OWNER";

struct Mark {
    path: String,
    dir_id: String,
    mark_id: String,
    machine_id: Option<String>,
}

/// Marks the server at `data_dir` as the mirror owner, surviving restarts.
pub fn mark(data_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let own = canonical(data_dir)?;
    let dir_id = file_id::path_identity(&own)?;
    let machine_id = file_id::machine_identity();
    let path = own.join(MARK_FILE);
    // Write via a same-directory temp file so the mark records its own
    // filesystem identity before atomic placement keeps that identity at
    // `MARK_FILE`.
    atomicwrites::AtomicFile::new(&path, atomicwrites::AllowOverwrite)
        .write(|file| {
            let mark_id = file_id::file_identity(file)?;
            let bytes = encode_mark(
                &own,
                dir_id.as_str(),
                mark_id.as_str(),
                machine_id.as_deref(),
            );
            file.write_all(&bytes)?;
            file.sync_all()
        })
        .map_err(|err| match err {
            atomicwrites::Error::Internal(io) | atomicwrites::Error::User(io) => io,
        })
}

/// Removes the mark, so the server at `data_dir` stops pushing.
pub fn unmark(data_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(data_dir.join(MARK_FILE)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// True when the mark binds this exact `data_dir`, mark file, and installation.
/// Any read, identity or parse failure counts as not the owner: a server that
/// cannot tell must not push.
pub fn is_owner(data_dir: &Path) -> bool {
    let Ok(own) = canonical(data_dir) else {
        return false;
    };
    let Ok(dir_id) = file_id::path_identity(&own) else {
        return false;
    };
    let mark_path = own.join(MARK_FILE);
    let Ok(meta) = std::fs::symlink_metadata(&mark_path) else {
        return false;
    };
    if !meta.file_type().is_file() {
        return false;
    }
    let Ok(file) = File::open(&mark_path) else {
        return false;
    };
    let Ok(mark_id) = file_id::file_identity(&file) else {
        return false;
    };
    let Ok(bytes) = read_bounded(file) else {
        return false;
    };
    let Some(mark) = decode_mark(&bytes) else {
        return false;
    };
    same_dir(Path::new(&mark.path), &own)
        && mark.dir_id == dir_id.as_str()
        && mark.mark_id == mark_id.as_str()
        && mark.machine_id == file_id::machine_identity()
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

fn encode_mark(path: &Path, dir_id: &str, mark_id: &str, machine_id: Option<&str>) -> Vec<u8> {
    let json = serde_json::json!({
        "version": MARK_VERSION,
        "path": path.to_string_lossy(),
        "dir_id": dir_id,
        "mark_id": mark_id,
        "machine_id": machine_id,
    });
    serde_json::to_vec(&json).expect("a JSON value always serialises")
}

fn decode_mark(bytes: &[u8]) -> Option<Mark> {
    let json: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if json.get("version")?.as_u64()? != MARK_VERSION {
        return None;
    }
    let path = json.get("path")?.as_str()?.to_owned();
    let dir_id = json.get("dir_id")?.as_str()?.to_owned();
    let mark_id = json.get("mark_id")?.as_str()?.to_owned();
    let machine_id = match json.get("machine_id")? {
        serde_json::Value::Null => None,
        serde_json::Value::String(id) => Some(id.clone()),
        _ => return None,
    };
    Some(Mark {
        path,
        dir_id,
        mark_id,
        machine_id,
    })
}

fn read_bounded(file: File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(MAX_MARK_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MARK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mirror-owner file exceeds size limit",
        ));
    }
    Ok(bytes)
}

fn canonical(dir: &Path) -> io::Result<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Risk (R1, #486): a copied data directory placed at the same canonical
    /// path, a copied mark file placed into an existing directory, or a legacy
    /// path-only mark must never be accepted as the mirror owner, while the
    /// marked directory itself must remain owner across restarts.
    #[test]
    fn same_path_copy_and_copied_mark_are_not_owners() {
        let temp = tempfile::tempdir().unwrap();
        let orig = temp.path().join("project");
        let copy = temp.path().join("copy");
        let saved = temp.path().join("saved");
        mark(&orig).unwrap();
        assert!(is_owner(&orig));

        // Same-path directory copy: identical path and mark bytes, new objects.
        std::fs::create_dir(&copy).unwrap();
        std::fs::copy(orig.join(MARK_FILE), copy.join(MARK_FILE)).unwrap();
        assert!(!is_owner(&copy));
        std::fs::rename(&orig, &saved).unwrap();
        std::fs::rename(&copy, &orig).unwrap();
        assert!(!is_owner(&orig));

        // Restoring the original directory keeps ownership; replacing its mark
        // file with a copy inside the same directory revokes it.
        std::fs::remove_dir_all(&orig).unwrap();
        std::fs::rename(&saved, &orig).unwrap();
        assert!(is_owner(&orig));
        let mark_bytes = std::fs::read(orig.join(MARK_FILE)).unwrap();
        unmark(&orig).unwrap();
        std::fs::write(orig.join(MARK_FILE), &mark_bytes).unwrap();
        assert!(!is_owner(&orig));

        // Legacy path-only marks fail closed.
        let legacy = canonical(&orig).unwrap();
        std::fs::write(orig.join(MARK_FILE), legacy.to_string_lossy().as_bytes()).unwrap();
        assert!(!is_owner(&orig));
    }
}
