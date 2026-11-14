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
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

#[cfg(windows)]
fn place(temp: &Path, path: &Path, replace: Replace) -> io::Result<()> {
    windows::move_write_through(temp, path, replace == Replace::Allow)
}

#[cfg(not(any(unix, windows)))]
fn place(_temp: &Path, _path: &Path, _replace: Replace) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Creates a new, uniquely named owner-only file in `path`'s directory, so the
/// final rename stays on one volume.
fn create_temp_beside(path: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
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
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, MoveFileExW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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
        let descriptor = OwnerOnlyDescriptor::for_current_user()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
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

    /// A protected DACL granting full access to the current user and SYSTEM
    /// only, so nothing is inherited from the data directory.
    struct OwnerOnlyDescriptor(PSECURITY_DESCRIPTOR);

    impl OwnerOnlyDescriptor {
        fn for_current_user() -> io::Result<Self> {
            let sid = current_user_sid()?;
            let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: `sddl` is NUL-terminated; on success the OS allocates
            // the descriptor with LocalAlloc, and Drop frees it.
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
    }

    impl Drop for OwnerOnlyDescriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }

    /// The current process user's SID as a string, for example `S-1-5-21-…`.
    fn current_user_sid() -> io::Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: GetCurrentProcess returns a pseudo-handle; `token` is closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
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
}
