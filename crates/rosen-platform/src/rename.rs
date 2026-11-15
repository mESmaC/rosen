//! Atomic publication without replacing another creator's destination.
use std::io;
use std::path::Path;

/// Renames `from` to `to` only if no entry exists at `to`, atomically.
/// An existing file, directory or symlink is never replaced. Unsupported
/// platforms and filesystems return an error rather than a replacing rename.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let (from, to) = (wide_absolute(from)?, wide_absolute(to)?);
    // SAFETY: both buffers are NUL-terminated paths, with no interior NUL,
    // and remain live for this synchronous call. Zero flags exclude both
    // replacement and a non-atomic cross-volume copy/delete fallback.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn wide_absolute(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let absolute = std::path::absolute(path)?;
    let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // Match std filesystem operations' support for paths beyond MAX_PATH.
    // Absolute normal paths have already had dot components resolved.
    let mut extended = if wide.starts_with(&[92, 92, 63, 92]) || wide.starts_with(&[92, 92, 46, 92])
    {
        wide
    } else if wide.starts_with(&[92, 92]) {
        let mut extended: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
        extended.extend_from_slice(&wide[2..]);
        extended
    } else {
        let mut extended: Vec<u16> = "\\\\?\\".encode_utf16().collect();
        extended.extend_from_slice(&wide);
        extended
    };
    extended.push(0);
    Ok(extended)
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};

    // Linux uses RENAME_NOREPLACE; Apple uses RENAME_EXCL. Never fall back
    // to renameat if the kernel or filesystem refuses the exclusive flag.
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE)?;
    Ok(())
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
)))]
pub fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform has no atomic no-replace rename primitive",
    ))
}
