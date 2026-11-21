//! Handle-based classification and read-only-safe removal on Windows.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo, GetFileInformationByHandleEx,
    SetFileInformationByHandle, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_BASIC_INFO, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
};

/// Returns the held object's reparse tag, or `None` for a plain object.
/// Open with FILE_FLAG_OPEN_REPARSE_POINT to query the link itself.
#[allow(unsafe_code)]
pub fn reparse_tag(file: &File) -> io::Result<Option<u32>> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: File owns a live handle throughout this synchronous call.
    // The writable repr(C) buffer has the size and layout the API requires.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileAttributeTagInfo,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.ReparseTag))
}

/// Marks the exact held file or empty directory for deletion on close.
///
/// When Windows denies disposition on a read-only object (such as a git pack
/// file), clears `FILE_ATTRIBUTE_READONLY` on the same handle and retries so
/// no second open can introduce a pathname race.
pub fn delete_on_close(file: &File) -> io::Result<()> {
    match set_disposition(file) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            if clear_readonly(file).unwrap_or(false) {
                return set_disposition(file);
            }
            Err(err)
        }
        Err(err) => Err(err),
    }
}

/// Removes a single file or file link, clearing `FILE_ATTRIBUTE_READONLY` on
/// access denied without following symlinks.
pub fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            if clear_readonly_path(path).unwrap_or(false) {
                return fs::remove_file(path);
            }
            Err(err)
        }
        Err(err) => Err(err),
    }
}

/// Removes an empty directory or directory link, clearing
/// `FILE_ATTRIBUTE_READONLY` on access denied without following junctions.
pub fn remove_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            if clear_readonly_path(path).unwrap_or(false) {
                return fs::remove_dir(path);
            }
            Err(err)
        }
        Err(err) => Err(err),
    }
}

/// Removes `path` and everything beneath it without following junctions or
/// symlinks, clearing `FILE_ATTRIBUTE_READONLY` whenever a delete fails with
/// access denied.
pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return remove_reparse_point(path, meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0);
    }
    if !meta.is_dir() {
        return remove_file(path);
    }
    let mut stack = vec![(path.to_path_buf(), false)];
    while let Some((dir, visited)) = stack.pop() {
        if visited {
            let result = remove_dir(&dir);
            if dir == path {
                result?;
            } else {
                ignore_not_found(result)?;
            }
            continue;
        }
        stack.push((dir.clone(), true));
        remove_dir_children(&dir, dir == path, &mut stack)?;
    }
    Ok(())
}

fn remove_dir_children(
    dir: &Path,
    is_root: bool,
    stack: &mut Vec<(PathBuf, bool)>,
) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if !is_root && err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let child = entry?.path();
        let meta = match fs::symlink_metadata(&child) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            let is_dir = meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0;
            ignore_not_found(remove_reparse_point(&child, is_dir))?;
        } else if meta.is_dir() {
            stack.push((child, false));
        } else {
            ignore_not_found(remove_file(&child))?;
        }
    }
    Ok(())
}

fn remove_reparse_point(path: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        remove_dir(path)
    } else {
        remove_file(path)
    }
}

fn ignore_not_found(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn clear_readonly_path(path: &Path) -> io::Result<bool> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    clear_readonly(&file)
}

#[allow(unsafe_code)]
fn set_disposition(file: &File) -> io::Result<()> {
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
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[allow(unsafe_code)]
fn clear_readonly(file: &File) -> io::Result<bool> {
    let mut info = FILE_BASIC_INFO::default();
    // SAFETY: File owns a live handle for this synchronous call, and info is
    // a writable repr(C) buffer of the exact size FileBasicInfo requires.
    let read_ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileBasicInfo,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if read_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.FileAttributes & FILE_ATTRIBUTE_READONLY == 0 {
        return Ok(false);
    }
    let cleared = info.FileAttributes & !FILE_ATTRIBUTE_READONLY;
    // Zero timestamps leave all four times unchanged per FILE_BASIC_INFO docs.
    let updated = FILE_BASIC_INFO {
        CreationTime: 0,
        LastAccessTime: 0,
        LastWriteTime: 0,
        ChangeTime: 0,
        FileAttributes: if cleared == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            cleared
        },
    };
    // SAFETY: File owns a live handle and updated is an initialized repr(C)
    // buffer valid for the duration of this synchronous call.
    let write_ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileBasicInfo,
            std::ptr::from_ref(&updated).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if write_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // R4 / #430: read-only files (such as git pack files) must be removed
    // without following directory junctions into targets outside the tree.
    #[test]
    fn remove_dir_all_clears_readonly_without_following_junctions() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        let pack = tree.join(".git/objects/pack/pack-00.idx");
        fs::create_dir_all(pack.parent().unwrap()).unwrap();
        fs::write(&pack, b"pack").unwrap();
        let mut perms = fs::metadata(&pack).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&pack, perms).unwrap();

        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let link = tree.join("junction");
        let status = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "New-Item -ItemType Junction -Path $env:L -Target $env:T -ErrorAction Stop | Out-Null",
            ])
            .env("L", &link)
            .env("T", &outside)
            .status()
            .unwrap();
        assert!(status.success());

        remove_dir_all(&tree).unwrap();
        assert!(!tree.exists());
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
    }
}
