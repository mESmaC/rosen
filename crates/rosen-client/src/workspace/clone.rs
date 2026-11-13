//! The ReFS block clone fast path (SPEC.md R3).
//!
//! A block clone makes a new file that shares the source's disk blocks until
//! either side writes, so cloning a whole workspace (with its ignored build
//! output) costs almost nothing. Unlike a hardlink, each clone is its own
//! file: a write to it copies the touched blocks and never reaches the
//! source.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use reflink_copy::ReflinkSupport;
use rosen_core::ObjectId;

use super::layout::{disk_path, FileEntry, FileKind};
use super::{parallel, MaterialiseError, Sparse};

/// What one clone pass did.
#[derive(Debug, Default)]
pub struct Cloned {
    /// Files cloned block for block.
    pub cloned: u64,
    /// Files the file system would not clone, plain-copied instead.
    pub copied: u64,
}

/// Every directory, file and link in a source workspace, as `/`-separated
/// paths from its root, parents before children.
#[derive(Debug, Default)]
struct Scan {
    dirs: Vec<Vec<u8>>,
    files: Vec<Vec<u8>>,
    links: Vec<Vec<u8>>,
}

/// Whether block clones from `source` into `dest_parent` might work: both are
/// on one volume whose file system clones. Outside Windows the answer is
/// unknown until a clone is tried, so the probe in [`clone_workspace`] decides.
pub fn may_block_clone(source: &Path, dest_parent: &Path) -> bool {
    !matches!(
        reflink_copy::check_reflink_support(source, dest_parent),
        Ok(ReflinkSupport::NotSupported) | Err(_)
    )
}

/// Clones every file under `source` that `sparse` includes into the empty
/// directory `dest`. `probe` is one tracked file of the source's tree; it is
/// cloned first and hashed, so a file system or driver that "clones" wrongly
/// is caught before a workspace is built on it.
pub fn clone_workspace(
    source: &Path,
    dest: &Path,
    sparse: &Sparse,
    probe: Option<&FileEntry>,
) -> Result<Cloned, MaterialiseError> {
    let scan = scan(source, sparse)?;
    for dir in &scan.dirs {
        let path = disk_path(dest, dir);
        fs::create_dir(&path).map_err(|error| MaterialiseError::io(&path, error))?;
    }
    if let Some(entry) = probe {
        probe_clone(source, dest, entry)?;
    }
    let probed = probe.map(|entry| entry.path.as_slice());
    let (cloned, copied) = (AtomicU64::new(0), AtomicU64::new(0));
    let files: Vec<&Vec<u8>> = scan
        .files
        .iter()
        .filter(|f| Some(f.as_slice()) != probed)
        .collect();
    parallel::for_each::<_, MaterialiseError, _>(&files, |path| {
        let counter = if clone_or_copy(source, dest, path)? {
            &cloned
        } else {
            &copied
        };
        counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    })?;
    for link in &scan.links {
        copy_link(&disk_path(source, link), &disk_path(dest, link))?;
    }
    Ok(Cloned {
        cloned: cloned.into_inner() + u64::from(probe.is_some()),
        copied: copied.into_inner(),
    })
}

/// Clones one file, or plain-copies it when the file system refuses the
/// clone (for example a file with a different integrity setting). Returns
/// whether it was cloned. Never hardlinks.
fn clone_or_copy(source: &Path, dest: &Path, path: &[u8]) -> Result<bool, MaterialiseError> {
    let (from, to) = (disk_path(source, path), disk_path(dest, path));
    if reflink_copy::reflink(&from, &to).is_ok() {
        return Ok(true);
    }
    // A failed clone removes its own partial file, so the copy starts clean.
    fs::copy(&from, &to).map_err(|error| MaterialiseError::io(&from, error))?;
    Ok(false)
}

/// Clones the probe file and checks that its bytes hash to its blob ID.
fn probe_clone(source: &Path, dest: &Path, entry: &FileEntry) -> Result<(), MaterialiseError> {
    let (from, to) = (disk_path(source, &entry.path), disk_path(dest, &entry.path));
    reflink_copy::reflink(&from, &to).map_err(|error| MaterialiseError::io(&from, error))?;
    let data = fs::read(&to).map_err(|error| MaterialiseError::io(&to, error))?;
    if ObjectId::hash_object("blob", &data) != entry.blob {
        return Err(MaterialiseError::CloneMismatch(to));
    }
    Ok(())
}

/// A tracked regular file to probe with: the first one in the layout.
pub fn pick_probe(files: &[FileEntry]) -> Option<&FileEntry> {
    files.iter().find(|entry| entry.kind != FileKind::Symlink)
}

fn scan(source: &Path, sparse: &Sparse) -> Result<Scan, MaterialiseError> {
    let mut scan = Scan::default();
    let mut pending = vec![Vec::new()];
    while let Some(dir) = pending.pop() {
        let disk = disk_path(source, &dir);
        let disk = if dir.is_empty() {
            source.to_path_buf()
        } else {
            disk
        };
        let entries = fs::read_dir(&disk).map_err(|error| MaterialiseError::io(&disk, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| MaterialiseError::io(&disk, error))?;
            let kind = entry
                .file_type()
                .map_err(|error| MaterialiseError::io(&entry.path(), error))?;
            let path = child_path(&dir, &entry.file_name(), &entry.path())?;
            if kind.is_symlink() {
                if sparse.includes(&path) {
                    scan.links.push(path);
                }
            } else if kind.is_dir() {
                if sparse.enters(&path) {
                    scan.dirs.push(path.clone());
                    pending.push(path);
                }
            } else if sparse.includes(&path) {
                scan.files.push(path);
            }
        }
    }
    Ok(scan)
}

fn child_path(
    dir: &[u8],
    name: &std::ffi::OsStr,
    full: &Path,
) -> Result<Vec<u8>, MaterialiseError> {
    let name = name_bytes(name)
        .ok_or_else(|| MaterialiseError::UnsupportedName(full.display().to_string()))?;
    let mut path = dir.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    Ok(path)
}

#[cfg(unix)]
fn name_bytes(name: &std::ffi::OsStr) -> Option<&[u8]> {
    use std::os::unix::ffi::OsStrExt;
    Some(name.as_bytes())
}

#[cfg(not(unix))]
fn name_bytes(name: &std::ffi::OsStr) -> Option<&[u8]> {
    name.to_str().map(str::as_bytes)
}

/// Recreates a link the source holds, such as one a prepare command made.
fn copy_link(from: &Path, to: &Path) -> Result<(), MaterialiseError> {
    let target = fs::read_link(from).map_err(|error| MaterialiseError::io(from, error))?;
    make_link(from, &target, to).map_err(|error| MaterialiseError::io(to, error))
}

#[cfg(unix)]
fn make_link(_from: &Path, target: &Path, to: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, to)
}

#[cfg(windows)]
fn make_link(from: &Path, target: &Path, to: &Path) -> io::Result<()> {
    if fs::metadata(from).is_ok_and(|meta| meta.is_dir()) {
        std::os::windows::fs::symlink_dir(target, to)
    } else {
        std::os::windows::fs::symlink_file(target, to)
    }
}

#[cfg(not(any(unix, windows)))]
fn make_link(_from: &Path, _target: &Path, _to: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
