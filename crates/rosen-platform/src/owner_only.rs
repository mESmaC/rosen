//! Files only the current OS user can open, for secrets such as the server's
//! TLS key and a machine's enrollment (SPEC.md R14).
//!
//! The file is created with its final access list in one call, so no other
//! user can open a handle during a window when it inherits the directory's
//! list. A handle's rights are checked only when it opens, so restricting the
//! file afterwards would not shut out a handle opened in that window.
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether `write` may replace an existing file at the target path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    Allow,
    /// The write fails with `AlreadyExists` and leaves the existing file.
    Refuse,
}

/// Atomically writes `bytes` to `path` as a file only the current user (and,
/// on Windows, SYSTEM) can open. Readers see the old file or the whole new one.
pub fn write(path: &Path, bytes: &[u8], replace: Replace) -> io::Result<()> {
    let (temp_path, mut temp) = create_temp_beside(path)?;
    let written = temp.write_all(bytes).and_then(|()| temp.sync_all());
    drop(temp);
    let placed = written.and_then(|()| place(&temp_path, path, replace));
    // Gone after a move; a Unix hard link or a failed placement leaves it.
    if temp_path.exists() {
        let _ = std::fs::remove_file(&temp_path);
    }
    placed
}

/// Moves the finished temp file to `path` and makes the new directory entry
/// durable, so a power loss after success cannot lose a written identity.
/// The placed file keeps the temp file's access list.
#[cfg(unix)]
fn place(temp: &Path, path: &Path, replace: Replace) -> io::Result<()> {
    match replace {
        Replace::Allow => std::fs::rename(temp, path)?,
        // Unix has no portable no-replace rename; a hard link fails if the
        // target exists.
        Replace::Refuse => std::fs::hard_link(temp, path)?,
    }
    File::open(parent_dir(path))?.sync_all()
}

#[cfg(windows)]
fn place(temp: &Path, path: &Path, replace: Replace) -> io::Result<()> {
    windows::move_write_through(temp, path, replace == Replace::Allow)
}

#[cfg(not(any(unix, windows)))]
fn place(_temp: &Path, _path: &Path, _replace: Replace) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The directory holding `path`. A bare file name has an empty parent, which
/// names no directory to open.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Creates a new, uniquely named owner-only file in `path`'s directory, so the
/// final rename stays on one volume.
fn create_temp_beside(path: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = parent_dir(path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    loop {
        let temp = dir.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match create_new(&temp) {
            Ok(file) => return Ok((temp, file)),
            // A leftover from a crashed process with a recycled ID.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn create_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn create_new(path: &Path) -> io::Result<File> {
    windows::create_new(path)
}

#[cfg(not(any(unix, windows)))]
fn create_new(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform cannot create owner-only files",
    ))
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use crate::security::{OwnerOnlyDescriptor, Rights};
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, MoveFileExW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if wide[..wide.len() - 1].contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(wide)
    }

    /// Without `replace` the move fails with `AlreadyExists` when `to` exists,
    /// atomically and with no hard-link support needed. Write-through returns
    /// only once the new directory entry is on disk.
    pub(super) fn move_write_through(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
        let (from, to) = (wide(from)?, wide(to)?);
        let flags = MOVEFILE_WRITE_THROUGH
            | if replace {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        // SAFETY: both paths are NUL-terminated and live for this synchronous call.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn create_new(path: &Path) -> io::Result<File> {
        // Nothing is inherited from the data directory.
        let descriptor = OwnerOnlyDescriptor::for_current_user(Rights::FileAll)?;
        let attributes = descriptor.attributes();
        let wide = wide(path)?;
        // SAFETY: `wide` is NUL-terminated and `attributes` points at a live
        // descriptor; both outlive this synchronous call. CREATE_NEW applies
        // the descriptor to the file it creates and fails if one exists.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW returned a valid handle this function owns.
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}
