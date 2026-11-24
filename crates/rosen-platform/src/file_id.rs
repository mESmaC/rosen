//! Stable filesystem object identity and OS machine identity for binding
//! per-installation state such as the mirror-owner mark (SPEC R1, #486).

use std::fs::File;
use std::io;
use std::path::Path;

/// Identifies one filesystem object (a file or directory) on its volume.
///
/// Copying a file or directory — even onto the same canonical path — creates
/// a new object with a different identity, while restarting on the original
/// object keeps the same identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity(String);

impl FileIdentity {
    /// Stable ASCII representation suitable for persistence and comparison.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Returns the filesystem object identity for an open file or directory handle.
pub fn file_identity(file: &File) -> io::Result<FileIdentity> {
    imp::file_identity(file)
}

/// Returns the filesystem object identity for the file or directory at `path`.
pub fn path_identity(path: &Path) -> io::Result<FileIdentity> {
    imp::path_identity(path)
}

/// Returns an OS-level installation identifier when the platform provides one
/// that persists across restarts without depending on the filesystem alone.
///
/// Windows volume serial numbers already distinguish volumes on different
/// installations, so this returns `None` there.
pub fn machine_identity() -> Option<String> {
    imp::machine_identity()
}

#[cfg(any(windows, target_os = "macos"))]
fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(windows)]
mod imp {
    use super::{hex_bytes, FileIdentity};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    pub(super) fn path_identity(path: &Path) -> io::Result<FileIdentity> {
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        file_identity(&file)
    }

    pub(super) fn file_identity(file: &File) -> io::Result<FileIdentity> {
        match file_id_info(file) {
            Ok(id) => Ok(id),
            Err(err) if is_file_id_info_unsupported(&err) => by_handle_info(file),
            Err(err) => Err(err),
        }
    }

    pub(super) fn machine_identity() -> Option<String> {
        None
    }

    fn is_file_id_info_unsupported(err: &io::Error) -> bool {
        matches!(
            err.raw_os_error().map(|code| code as u32),
            Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
        )
    }

    #[allow(unsafe_code)]
    fn file_id_info(file: &File) -> io::Result<FileIdentity> {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: `file` owns a live handle for this synchronous call, and
        // `info` is a writable `repr(C)` buffer matching `FileIdInfo`.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle().cast(),
                FileIdInfo,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileIdentity(format!(
            "win128:{:016x}:{}",
            info.VolumeSerialNumber,
            hex_bytes(&info.FileId.Identifier)
        )))
    }

    #[allow(unsafe_code)]
    fn by_handle_info(file: &File) -> io::Result<FileIdentity> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a live handle for this synchronous call, and
        // `info` is a writable `repr(C)` buffer of the exact required type.
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok(FileIdentity(format!(
            "win64:{:08x}:{index:016x}",
            info.dwVolumeSerialNumber
        )))
    }
}

#[cfg(unix)]
mod imp {
    use super::FileIdentity;
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub(super) fn path_identity(path: &Path) -> io::Result<FileIdentity> {
        let meta = std::fs::metadata(path)?;
        Ok(FileIdentity(format!(
            "unix:{:016x}:{:016x}",
            meta.dev(),
            meta.ino()
        )))
    }

    pub(super) fn file_identity(file: &File) -> io::Result<FileIdentity> {
        let meta = file.metadata()?;
        Ok(FileIdentity(format!(
            "unix:{:016x}:{:016x}",
            meta.dev(),
            meta.ino()
        )))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn machine_identity() -> Option<String> {
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(raw) = std::fs::read_to_string(path) {
                let id = raw.trim();
                if !id.is_empty() {
                    return Some(format!("linux:{id}"));
                }
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    pub(super) fn machine_identity() -> Option<String> {
        let mut uuid = [0u8; 16];
        let timeout = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        // SAFETY: `uuid` is a writable 16-byte buffer (`uuid_t`) and `timeout`
        // points to an initialized `timespec` valid for this synchronous call.
        if unsafe { libc::gethostuuid(uuid.as_mut_ptr(), &timeout) } == 0 {
            return Some(format!("macos:{}", super::hex_bytes(&uuid)));
        }
        None
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn machine_identity() -> Option<String> {
        None
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use super::FileIdentity;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub(super) fn path_identity(_path: &Path) -> io::Result<FileIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform has no file identity primitive",
        ))
    }

    pub(super) fn file_identity(_file: &File) -> io::Result<FileIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform has no file identity primitive",
        ))
    }

    pub(super) fn machine_identity() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Risk (R1, #486): mirror-owner verification depends on a copied directory
    /// or mark file moved onto the original path having a distinct identity
    /// from the original, while an atomic same-directory rename preserves the
    /// handle's identity.
    #[test]
    fn identity_survives_rename_and_distinguishes_same_path_copy() {
        let temp = tempfile::tempdir().unwrap();
        let orig = temp.path().join("project");
        let copy = temp.path().join("copy");
        let saved = temp.path().join("saved");
        std::fs::create_dir(&orig).unwrap();
        std::fs::create_dir(&copy).unwrap();

        let orig_dir_id = path_identity(&orig).unwrap();
        let temp_file = orig.join("mark.tmp");
        let mark_file = orig.join("mirror-owner");
        let handle = File::create(&temp_file).unwrap();
        let temp_file_id = file_identity(&handle).unwrap();
        drop(handle);
        std::fs::rename(&temp_file, &mark_file).unwrap();
        assert_eq!(path_identity(&mark_file).unwrap(), temp_file_id);

        std::fs::copy(&mark_file, copy.join("mirror-owner")).unwrap();
        std::fs::rename(&orig, &saved).unwrap();
        std::fs::rename(&copy, &orig).unwrap();

        assert_ne!(path_identity(&orig).unwrap(), orig_dir_id);
        assert_ne!(
            path_identity(&orig.join("mirror-owner")).unwrap(),
            temp_file_id
        );
    }
}
