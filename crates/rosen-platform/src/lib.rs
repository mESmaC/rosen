//! Small operating-system primitives with safe, owned-handle interfaces.
#![deny(unsafe_code)]

/// Marks the exact held file for deletion when its handles close.
///
/// Callers can open the file denying write/delete sharing before hashing it,
/// then delete that same file without reopening a replaceable pathname.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn delete_on_close(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: File owns a live Windows handle for the entire call. The
    // initialized repr(C) buffer matches FileDispositionInfo exactly; its
    // address and size remain valid until the synchronous call returns.
    // The kernel checks DELETE access; no caller can supply a raw pointer.
    let success = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileDispositionInfo,
            std::ptr::from_ref(&disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if success == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn delete_on_close(_file: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no verified handle-deletion primitive",
    ))
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

pub mod owner_only;

/// False only when no running process has id `pid`; any doubt answers true,
/// so a caller that cleans up after dead processes never touches a live
/// one's files. A reused id reads as running, which only delays cleanup.
#[cfg(windows)]
pub fn process_may_be_running(pid: u32) -> bool {
    process::may_be_running(pid)
}

/// Without a Windows process handle, Linux's `/proc` answers; other systems
/// cannot tell here, so every process counts as running. An entry `/proc`
/// refuses to stat, as under `hidepid`, counts as running too.
#[cfg(not(windows))]
pub fn process_may_be_running(pid: u32) -> bool {
    let proc_root = std::path::Path::new("/proc");
    !proc_root.join("self").exists()
        || proc_root
            .join(pid.to_string())
            .try_exists()
            .unwrap_or(true)
}
