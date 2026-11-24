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

pub mod file_id;

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
/// and every other uncertainty keep the process's staging directories. A
/// zombie has already exited and released its resources, waiting only to be
/// reaped by its parent, so it counts as not running (matching Windows'
/// `STILL_ACTIVE` check while a handle remains open, #438).
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
    if unsafe { libc::kill(pid, 0) } == -1 {
        return std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    }
    !is_zombie(pid)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn is_zombie(pid: libc::pid_t) -> bool {
    // SAFETY: `proc_bsdinfo` is a plain C data buffer; zero is valid for all
    // integer fields.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // Passing `arg = 1` enables XNU's `findzomb` fallback (`proc_find_zombref`
    // in `bsd/kern/proc_info.c`), since `proc_find` alone skips zombies.
    // SAFETY: `info` is a valid writable buffer of `size` bytes for the
    // synchronous call.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };
    written == size && info.pbi_status == libc::SZOMB
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: libc::pid_t) -> bool {
    // Read raw bytes because `/proc/<pid>/stat` embeds the process's raw
    // `comm` bytes inside `(...)`, which need not be valid UTF-8 (#458).
    let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else {
        return false;
    };
    proc_stat_is_zombie(&stat)
}

#[cfg(any(target_os = "linux", test))]
fn proc_stat_is_zombie(stat: &[u8]) -> bool {
    let Some(close) = stat.iter().rposition(|&byte| byte == b')') else {
        return false;
    };
    let rest = &stat[close + 1..];
    matches!(
        rest.trim_ascii_start()
            .split(u8::is_ascii_whitespace)
            .next(),
        Some(b"Z" | b"X" | b"x")
    )
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn is_zombie(_pid: libc::pid_t) -> bool {
    false
}

#[cfg(not(any(windows, unix)))]
pub fn process_may_be_running(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Risk (#438): `rosen server stop` polls `process_may_be_running` until
    /// the daemon exits. If an exited child not yet reaped by its parent (a
    /// zombie on Unix, or an open handle on Windows) still reports as running,
    /// `server stop` deadlocks when called before the parent reaps the child.
    #[test]
    fn exited_unreaped_child_is_not_running() {
        assert!(process_may_be_running(std::process::id()));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_may_be_running(pid) {
            assert!(
                Instant::now() < deadline,
                "exited unreaped child {pid} still reported as running"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait().expect("reap child");
        assert!(status.success(), "child failed: {status}");
    }

    /// Risk (#458, R10): `/proc/<pid>/stat` embeds raw `comm` bytes, so a
    /// non-UTF-8 process name must still report zombie/dead states while
    /// malformed records stay conservative.
    #[test]
    fn proc_stat_handles_non_utf8_process_name() {
        assert!(proc_stat_is_zombie(b"496 (rosen-\xff) Z 1 2 3\n"));
        assert!(proc_stat_is_zombie(b"496 (a)\xff) X 1\n"));
        assert!(proc_stat_is_zombie(b"496 (rosen-\xff) x 1\n"));
        assert!(!proc_stat_is_zombie(b"496 (rosen-\xff) Z) S 1\n"));
        assert!(!proc_stat_is_zombie(b"496 (rosen-\xff)"));
        assert!(!proc_stat_is_zombie(b"496 (rosen-\xff"));
    }
}
