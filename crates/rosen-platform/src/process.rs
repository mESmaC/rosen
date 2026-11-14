//! Starts a detached background process that inherits no handles, and tells
//! whether a process id may still name a running process.
//!
//! `std::process::Command` always calls `CreateProcessW` with handle
//! inheritance on, so a child inherits every inheritable handle in the parent,
//! including a pipe the shell passed as the parent's stdout. A long-lived daemon
//! holding that pipe's write end keeps `rosen ... | cat` from ever seeing end of
//! input (issue #37). Spawning with inheritance off avoids that.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::{io, ptr};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, OpenProcess, WaitForSingleObject,
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// A process started by [`spawn_detached`]; dropping it closes the handle
/// without stopping the process.
#[derive(Debug)]
pub struct DetachedProcess {
    process: OwnedHandle,
}

impl DetachedProcess {
    /// The exit code once the process has exited, else `None`, without blocking.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        let handle = self.process.as_raw_handle();
        // SAFETY: `process` owns a live process handle for this whole call.
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_TIMEOUT => return Ok(None),
            WAIT_OBJECT_0 => {}
            _ => return Err(io::Error::last_os_error()),
        }
        let mut code = 0u32;
        // SAFETY: the handle is live and `code` is a valid out pointer.
        if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(code))
    }
}

/// False only when no running process has id `pid`. Every doubt answers
/// true: a process this user may not open, or one that exited with the
/// code that also means "still active", counts as running.
pub fn may_be_running(pid: u32) -> bool {
    // SAFETY: OpenProcess takes plain values and returns a new handle or null.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        // Windows reports an id that names no process as an invalid parameter.
        let error = io::Error::last_os_error();
        return error.raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32);
    }
    // SAFETY: a non-null OpenProcess result is a fresh handle this function
    // owns and nothing else closes.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut code = 0u32;
    // SAFETY: the handle is live and `code` is a valid out pointer. A process
    // object outlives its process while handles to it remain, so the exit
    // code, not the open, tells whether it still runs.
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
        return true;
    }
    code == STILL_ACTIVE as u32
}

/// Starts `program` with `args`, detached from the caller's console and
/// process group, with no inherited handles and null standard handles.
/// `leave_job` also takes it out of the caller's job object; spawning then
/// fails when the job forbids breakaway. The child keeps the caller's
/// environment and current directory. `program` is used as given, without a
/// PATH search.
pub fn spawn_detached(
    program: &Path,
    args: &[&OsStr],
    leave_job: bool,
) -> io::Result<DetachedProcess> {
    let application = terminated(program.as_os_str())?;
    let mut command_line = command_line(program.as_os_str(), args)?;
    let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    if leave_job {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    // Explicit null standard handles: without STARTF_USESTDHANDLES Windows
    // copies the parent's handle values into the child, where they would name
    // whatever the child happens to open later.
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION {
        hProcess: ptr::null_mut(),
        hThread: ptr::null_mut(),
        dwProcessId: 0,
        dwThreadId: 0,
    };
    // SAFETY: both strings are NUL-terminated buffers that outlive the call,
    // and the command line is mutable as CreateProcessW requires. The startup
    // and information structs are initialized repr(C) values sized for the
    // call. Inheritance is off (FALSE), so no handle crosses to the child.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            flags,
            ptr::null(),
            ptr::null(),
            &startup,
            &mut info,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful CreateProcessW returns two fresh handles this
    // process owns and nothing else closes.
    let (process, _thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(info.hProcess),
            OwnedHandle::from_raw_handle(info.hThread),
        )
    };
    Ok(DetachedProcess { process })
}

fn terminated(text: &OsStr) -> io::Result<Vec<u16>> {
    nul_terminated(text.encode_wide().collect())
}

fn nul_terminated(mut wide: Vec<u16>) -> io::Result<Vec<u16>> {
    if wide.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    wide.push(0);
    Ok(wide)
}

/// Builds a command line the Microsoft C runtime splits back into exactly
/// `program` and `args`.
fn command_line(program: &OsStr, args: &[&OsStr]) -> io::Result<Vec<u16>> {
    let mut line = Vec::new();
    // The program name ends at the next quote, with no backslash escapes, and
    // a Windows path cannot contain a quote.
    line.push(u16::from(b'"'));
    line.extend(program.encode_wide());
    line.push(u16::from(b'"'));
    for arg in args {
        line.push(u16::from(b' '));
        append_quoted(arg, &mut line);
    }
    nul_terminated(line)
}

fn append_quoted(arg: &OsStr, line: &mut Vec<u16>) {
    let wide: Vec<u16> = arg.encode_wide().collect();
    let special = |c: &u16| b" \t\"".iter().any(|&byte| u16::from(byte) == *c);
    if !wide.is_empty() && !wide.iter().any(special) {
        line.extend(wide);
        return;
    }
    let backslash = u16::from(b'\\');
    let quote = u16::from(b'"');
    line.push(quote);
    // Backslashes are literal unless they precede a quote; then each one must
    // be doubled, and the quote itself escaped with one more.
    let mut backslashes = 0;
    for unit in wide {
        if unit == backslash {
            backslashes += 1;
        } else {
            if unit == quote {
                line.extend(std::iter::repeat_n(backslash, backslashes + 1));
            }
            backslashes = 0;
        }
        line.push(unit);
    }
    line.extend(std::iter::repeat_n(backslash, backslashes));
    line.push(quote);
}
