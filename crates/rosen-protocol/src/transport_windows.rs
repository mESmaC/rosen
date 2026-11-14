//! Named-pipe transport. Each pipe instance serves one client, so the listener
//! keeps one idle instance ready and creates the next as soon as a client takes it.

use super::BoxStream;
use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::time::Duration;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, HANDLE, HLOCAL,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

const PIPE_PREFIX: &str = r"\\.\pipe\";

/// How long a client keeps retrying while every pipe instance is busy.
const BUSY_RETRY_LIMIT: Duration = Duration::from_secs(5);

pub(super) fn normalize(name: String) -> String {
    if name.starts_with(PIPE_PREFIX) {
        name
    } else {
        format!("{PIPE_PREFIX}{name}")
    }
}

/// The pipe namespace is machine-wide, so the default name carries the user's
/// SID: unlike `USERNAME`, it is unique across local and domain accounts.
pub(super) fn default_endpoint() -> String {
    let user = current_user_sid()
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".into());
    format!("{PIPE_PREFIX}rosen-{user}")
}

pub(super) struct Listener {
    name: String,
    security: OwnerOnlySecurity,
    next: NamedPipeServer,
}

impl Listener {
    pub(super) fn bind(name: &str) -> io::Result<Self> {
        let security = OwnerOnlySecurity::for_current_user()?;
        let first = create_instance(name, &security, true).map_err(|err| {
            // A pipe that already exists refuses a "first instance" create.
            if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{name} is already in use"),
                )
            } else {
                err
            }
        })?;
        Ok(Self {
            name: name.to_owned(),
            security,
            next: first,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<BoxStream> {
        self.next.connect().await?;
        // Create the replacement before handing off, so a client arriving now
        // finds an instance instead of "not found".
        let replacement = create_instance(&self.name, &self.security, false)?;
        let connected = std::mem::replace(&mut self.next, replacement);
        Ok(Box::new(connected))
    }
}

fn create_instance(
    name: &str,
    security: &OwnerOnlySecurity,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut attributes = security.attributes();
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: `attributes` points at a security descriptor owned by `security`,
    // which outlives this call; the OS copies it into the new pipe object.
    unsafe {
        options.create_with_security_attributes_raw(
            name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
}

pub(super) async fn connect(name: &str) -> io::Result<BoxStream> {
    let started = tokio::time::Instant::now();
    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => {
                verify_server_owner(name, &client)?;
                return Ok(Box::new(client));
            }
            // Every instance is taken for the moment; the listener makes a new one
            // right after each accept, so a short wait is enough.
            Err(err)
                if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && started.elapsed() < BUSY_RETRY_LIMIT =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Refuses a pipe whose server process runs as another user. Pipe names are
/// machine-wide, so another account can create ours first and impersonate the
/// daemon; the DACL the real server sets cannot stop that, only this check can.
fn verify_server_owner(name: &str, pipe: &NamedPipeClient) -> io::Result<()> {
    let mut pid = 0u32;
    // SAFETY: the handle is a connected pipe client, alive for this call.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let owner = process_id_user_sid(pid).map_err(|err| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("cannot verify the owner of {name} (server process {pid}): {err}"),
        )
    })?;
    if owner != current_user_sid()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{name} is served by another user ({owner}, process {pid}); refusing to connect"),
        ));
    }
    Ok(())
}

/// A security descriptor granting full access to the current user and SYSTEM
/// only. The default pipe DACL also grants read access to Everyone, which would
/// break the "access is limited to the OS user" rule.
struct OwnerOnlySecurity {
    descriptor: PSECURITY_DESCRIPTOR,
}

// SAFETY: the descriptor is immutable after creation and only read by the OS.
unsafe impl Send for OwnerOnlySecurity {}
unsafe impl Sync for OwnerOnlySecurity {}

impl OwnerOnlySecurity {
    fn for_current_user() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; on success the OS allocates the
        // descriptor with LocalAlloc, and Drop frees it with LocalFree.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.descriptor as HLOCAL) };
    }
}

/// The current process user's SID as a string, for example `S-1-5-21-…`.
fn current_user_sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    process_user_sid(unsafe { GetCurrentProcess() })
}

/// The user SID of the process with id `pid`.
fn process_id_user_sid(pid: u32) -> io::Result<String> {
    // Limited query rights are granted even across integrity levels.
    // SAFETY: plain call; the returned handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let sid = process_user_sid(process);
    // SAFETY: `process` was opened above and is not used after this.
    unsafe { CloseHandle(process) };
    sid
}

fn process_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is a valid process handle; `token` is closed below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid = token_user_sid(token);
    // SAFETY: `token` was opened above and is not used after this.
    unsafe { CloseHandle(token) };
    sid
}

fn token_user_sid(token: HANDLE) -> io::Result<String> {
    let mut needed = 0u32;
    // SAFETY: a size query with a null buffer; it fails and reports `needed`.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    // u64 storage keeps the buffer aligned for TOKEN_USER.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` holds at least `needed` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation filled the buffer with a TOKEN_USER.
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let mut wide: *mut u16 = std::ptr::null_mut();
    // SAFETY: the SID points into `buffer`, which is alive; `wide` is freed below.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `wide` is a NUL-terminated string allocated by the OS.
    let sid = unsafe {
        let len = (0..).take_while(|&i| *wide.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(wide, len))
    };
    // SAFETY: allocated by ConvertSidToStringSidW with LocalAlloc.
    unsafe { LocalFree(wide as HLOCAL) };
    Ok(sid)
}
