//! Writing tracked files from the local object store.
//!
//! Every file is created new, with its own data. Nothing here links to the
//! store or to another workspace, so a tool writing a file in place changes
//! only that one file (SPEC.md R3, hardlinks are never used).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use rosen_store::ObjectStore;

use super::layout::{disk_path, FileEntry, FileKind};
use super::MaterialiseError;

/// Writes one tracked file under `root` from its blob, returning its size.
pub fn write_entry(
    store: &dyn ObjectStore,
    root: &Path,
    entry: &FileEntry,
) -> Result<u64, MaterialiseError> {
    let blob = store.read_blob(&entry.blob)?;
    let path = disk_path(root, &entry.path);
    let written = match entry.kind {
        FileKind::Symlink => write_symlink(&path, &blob.data),
        kind => write_file(&path, &blob.data, kind == FileKind::Executable),
    };
    written.map_err(|source| MaterialiseError::io(&path, source))?;
    Ok(blob.data.len() as u64)
}

fn write_file(path: &Path, data: &[u8], executable: bool) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_mode(&mut options, executable);
    let mut file: File = options.open(path)?;
    file.write_all(data)
}

#[cfg(unix)]
fn set_mode(options: &mut OpenOptions, executable: bool) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(if executable { 0o777 } else { 0o666 });
}

/// Windows has no executable bit; the mode stays in the tree.
#[cfg(not(unix))]
fn set_mode(_options: &mut OpenOptions, _executable: bool) {}

#[cfg(unix)]
fn write_symlink(path: &Path, target: &[u8]) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), path)
}

/// Windows symlinks need a privilege agents rarely hold, and must say up front
/// whether they point at a file or a directory. Like git's default on Windows
/// (`core.symlinks=false`), the link is written as a plain file holding its
/// target; the tree keeps the symlink mode.
#[cfg(not(unix))]
fn write_symlink(path: &Path, target: &[u8]) -> io::Result<()> {
    write_file(path, target, false)
}

/// Removes a tracked file so it can be written again. A missing file is fine:
/// the tree may name a file the source workspace never held.
pub fn remove_file(root: &Path, path: &[u8]) -> Result<(), MaterialiseError> {
    let path = disk_path(root, path);
    match fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(MaterialiseError::io(&path, error))
        }
        _ => Ok(()),
    }
}
