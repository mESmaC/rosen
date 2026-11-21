//! Small operating-system primitives with safe, owned-handle interfaces.
#![deny(unsafe_code)]

mod rename;
pub use rename::rename_noreplace;

#[cfg(windows)]
mod windows_delete;
#[cfg(windows)]
pub use windows_delete::{delete_on_close, remove_dir, remove_dir_all, remove_file, reparse_tag};

#[cfg(not(windows))]
pub fn delete_on_close(_file: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no verified handle-deletion primitive",
    ))
}

#[cfg(not(windows))]
pub fn remove_file(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(not(windows))]
pub fn remove_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::remove_dir(path)
}

#[cfg(not(windows))]
pub fn remove_dir_all(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(path)
}

/// Sends everything this process writes to standard error from now on,
/// including panic messages, to `file`. Rust looks the standard handle up on
/// each write, so `eprintln!` follows. The handle stays open for the rest of
/// the process, as a standard handle must.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn redirect_stderr(file: std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};
    let handle = file.into_raw_handle();
    // SAFETY: `handle` is a live file handle whose ownership this process
    // just released from `file`, so nothing else closes it while it is the
    // standard error handle.
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, handle.cast()) } == 0 {
        let error = std::io::Error::last_os_error();
        // SAFETY: the call failed, so the handle is still only ours to close.
        drop(unsafe { OwnedHandle::from_raw_handle(handle) });
        return Err(error);
    }
    Ok(())
}

/// Sends everything this process writes to standard error from now on,
/// including panic messages, to `file`.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn redirect_stderr(file: std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: both descriptors are open for the whole call. dup2 replaces
    // descriptor 2 with a copy of `file`'s, which then closes on drop.
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod credentials;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod process;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod run_tree;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod security;

pub mod owner_only;

pub mod memory;

#[cfg(unix)]
pub mod unix_delete;

/// False only when no running process has id `pid`; any doubt answers true,
/// so a caller that cleans up after dead processes never touches a live
/// one's files. A reused id reads as running, which only delays cleanup.
#[cfg(windows)]
pub fn process_may_be_running(pid: u32) -> bool {
    process::may_be_running(pid)
}

/// Signal zero probes existence without sending a signal. Permission errors
/// and every other uncertainty keep the process's staging directories.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn process_may_be_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    // SAFETY: a positive pid selects one process; signal zero sends nothing.
    (unsafe { libc::kill(pid, 0) != -1 })
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(any(windows, unix)))]
pub fn process_may_be_running(_pid: u32) -> bool {
    true
}
