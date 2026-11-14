//! Windows account identity and owner-only security descriptors, shared by
//! the owner-only files and the local named-pipe transport, so this unsafe
//! ABI code has one audited copy (issue #218).

use std::io;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
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

/// The access an owner-only descriptor grants its two trustees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rights {
    /// `GA`, generic all: for kernel objects such as a named pipe.
    GenericAll,
    /// `FA`, file all access: for files.
    FileAll,
}

/// A protected DACL granting `Rights` to the current user and SYSTEM only,
/// so nothing is inherited and no one else, Everyone included, gets access.
pub struct OwnerOnlyDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by the OS.
unsafe impl Send for OwnerOnlyDescriptor {}
unsafe impl Sync for OwnerOnlyDescriptor {}

impl OwnerOnlyDescriptor {
    pub fn for_current_user(rights: Rights) -> io::Result<Self> {
        let sid = current_user_sid()?;
        let right = match rights {
            Rights::GenericAll => "GA",
            Rights::FileAll => "FA",
        };
        let sddl: Vec<u16> = format!("D:P(A;;{right};;;{sid})(A;;{right};;;SY)")
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
        Ok(Self(descriptor))
    }

    /// Attributes naming this descriptor, valid while it lives. The OS copies
    /// the descriptor into each object created with them.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for OwnerOnlyDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

/// The current process user's SID as a string, for example `S-1-5-21-…`.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    process_handle_user_sid(unsafe { GetCurrentProcess() })
}

/// The user SID of the process with id `pid`.
pub fn process_user_sid(pid: u32) -> io::Result<String> {
    // Limited query rights are granted even across integrity levels.
    // SAFETY: plain call; the returned handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let sid = process_handle_user_sid(process);
    // SAFETY: `process` was opened above and is not used after this.
    unsafe { CloseHandle(process) };
    sid
}

/// The id of the process serving the named pipe `client` is connected to.
pub fn named_pipe_server_pid(client: &impl AsRawHandle) -> io::Result<u32> {
    let mut pid = 0u32;
    // SAFETY: the handle belongs to `client`, borrowed for this call.
    if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

fn process_handle_user_sid(process: HANDLE) -> io::Result<String> {
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
