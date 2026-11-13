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

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod credentials;
