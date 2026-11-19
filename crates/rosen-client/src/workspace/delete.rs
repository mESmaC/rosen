//! Platform-specific removal of verified workspace objects.
#[cfg(any(windows, unix))]
use crate::snapshot::is_ignore_file;
use rosen_core::ObjectId;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

pub(super) fn path_summary(label: &str, paths: &[String]) -> String {
    const LIMIT: usize = 5;
    let count = paths.len();
    let prefix = if count > LIMIT { "e.g. " } else { "" };
    let sample = paths[..count.min(LIMIT)].join(", ");
    format!("{label} ({count}): {prefix}{sample}")
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn remove_verified(
    _root: &Path,
    _expected: &BTreeMap<String, ObjectId>,
    _ignored: impl FnMut(&str, bool) -> bool,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "workspace kept; this platform cannot delete a verified file by handle",
    ))
}

#[cfg(unix)]
#[path = "delete/unix.rs"]
mod unix;

#[cfg(unix)]
pub(crate) use unix::remove_verified;

#[cfg(windows)]
pub(crate) fn remove_verified(
    root: &Path,
    expected: &BTreeMap<String, ObjectId>,
    ignored: impl FnMut(&str, bool) -> bool,
) -> io::Result<()> {
    windows::remove_verified(root, expected, ignored)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::io::Read;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;

    const SHARE_READ: u32 = 1;
    const SHARE_WRITE: u32 = 2;
    const DELETE: u32 = 0x0001_0000;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const REPARSE_POINT: u32 = 0x400;
    const MOUNT_POINT_TAG: u32 = 0xa000_0003;
    const SYMLINK_TAG: u32 = 0xa000_000c;

    struct CheckedFile {
        rel: String,
        file: File,
    }

    struct Plan {
        files: Vec<CheckedFile>,
        // Keep the checked directory identities pinned until each is empty.
        dirs: Vec<(PathBuf, File)>,
    }

    pub(super) fn remove_verified(
        root: &Path,
        expected: &BTreeMap<String, ObjectId>,
        mut ignored: impl FnMut(&str, bool) -> bool,
    ) -> io::Result<()> {
        let mut plan = plan(root, expected, &mut ignored)?;
        let (state, mut user): (Vec<_>, Vec<_>) = plan
            .files
            .drain(..)
            .partition(|entry| entry.rel.starts_with(".rosen/"));
        // Keep the rules available if removing build output is interrupted.
        user.sort_by_key(|entry| is_ignore_file(&entry.rel));
        for entry in user {
            delete_checked(entry)?;
        }
        plan.dirs
            .sort_by_key(|(dir, _)| std::cmp::Reverse(dir.components().count()));
        let (state_dirs, user_dirs): (Vec<_>, Vec<_>) = plan
            .dirs
            .into_iter()
            .partition(|(dir, _)| dir == root || dir.starts_with(root.join(".rosen")));
        for (dir, handle) in user_dirs {
            delete_directory(root, &dir, handle)?;
        }
        // Disposition on a directory is nonrecursive: a new file prevents
        // its removal. Keep the recovery marker until ordinary entries go.
        let remaining: Vec<_> = remaining_names(root, root)?
            .into_iter()
            .filter(|name| name != ".rosen")
            .collect();
        if !remaining.is_empty() {
            let summary = path_summary("new files appeared during deletion", &remaining);
            return Err(io::Error::other(format!("workspace kept; {summary}")));
        }
        let mut state = state;
        state.sort_by_key(|entry| entry.rel == ".rosen/workspace.json");
        for entry in state {
            delete_checked(entry)?;
        }
        for (dir, handle) in state_dirs {
            delete_directory(root, &dir, handle)?;
        }
        Ok(())
    }

    fn delete_directory(root: &Path, path: &Path, handle: File) -> io::Result<()> {
        if let Err(err) = rosen_platform::delete_on_close(&handle) {
            let names = match remaining_names(root, path) {
                Ok(names) => path_summary("remaining entries", &names),
                Err(listing) => format!("could not list remaining entries: {listing}"),
            };
            return Err(named(
                root,
                path,
                io::Error::new(err.kind(), format!("{err}; {names}")),
            ));
        }
        drop(handle);
        Ok(())
    }

    fn remaining_names(root: &Path, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)? {
            names.push(relative(root, &entry?.path())?);
        }
        names.sort();
        Ok(names)
    }
    fn plan(
        root: &Path,
        expected: &BTreeMap<String, ObjectId>,
        ignored: &mut impl FnMut(&str, bool) -> bool,
    ) -> io::Result<Plan> {
        let mut plan = Plan {
            files: Vec::new(),
            dirs: Vec::new(),
        };
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let handle = pin_directory(&dir).map_err(|err| named(root, &dir, err))?;
            if dir != root && dir.join(".rosen/workspace.json").try_exists()? {
                return Err(io::Error::other(format!(
                    "workspace kept; nested workspace: {}",
                    dir.display()
                )));
            }
            plan.dirs.push((dir.clone(), handle));
            for entry in fs::read_dir(&dir).map_err(|err| named(root, &dir, err))? {
                let path = entry?.path();
                let rel = relative(root, &path)?;
                let meta = fs::symlink_metadata(&path).map_err(|err| named(root, &path, err))?;
                if meta.file_attributes() & REPARSE_POINT != 0 {
                    plan.files.push(check_link(&path, rel, expected, ignored)?);
                } else if meta.is_dir() {
                    pending.push(path);
                } else {
                    // Ignore classification is fixed for this deletion plan;
                    // ignored output is outside R4 durability requirements.
                    let skip_hash = rel.starts_with(".rosen/") || ignored(&rel, false);
                    plan.files
                        .push(check_file(&path, rel, expected, skip_hash)?);
                }
            }
        }
        Ok(plan)
    }

    fn check_link(
        path: &Path,
        rel: String,
        expected: &BTreeMap<String, ObjectId>,
        ignored: &mut impl FnMut(&str, bool) -> bool,
    ) -> io::Result<CheckedFile> {
        // Deny writes and renames until disposal, so classification and the
        // target text cannot change after we verify this exact link object.
        let file = OpenOptions::new()
            .access_mode(0x8000_0000 | DELETE)
            .share_mode(SHARE_READ)
            .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|err| file_error(&rel, err))?;
        let tag = rosen_platform::reparse_tag(&file).map_err(|err| file_error(&rel, err))?;
        if !matches!(tag, Some(MOUNT_POINT_TAG | SYMLINK_TAG)) {
            return Err(io::Error::other(format!(
                "workspace kept; cannot verify reparse point: {rel} (tag {tag:?})"
            )));
        }
        // A directory link is a leaf: neither enumeration nor deletion ever
        // enters the target, which may belong to another workspace or store.
        if !ignored(&rel, false) && !rel.starts_with(".rosen/") {
            let target = fs::read_link(path).map_err(|err| file_error(&rel, err))?;
            let bytes = target.to_string_lossy().replace('\\', "/").into_bytes();
            if expected.get(&rel) != Some(&ObjectId::hash_object("blob", &bytes)) {
                return Err(io::Error::other(format!(
                    "workspace kept; unstored link: {rel}"
                )));
            }
        }
        Ok(CheckedFile { rel, file })
    }

    fn pin_directory(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .access_mode(0x8000_0000 | DELETE)
            .share_mode(SHARE_READ | SHARE_WRITE)
            .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_dir() || meta.file_attributes() & REPARSE_POINT != 0 {
            return Err(io::Error::other("not a plain directory"));
        }
        Ok(file)
    }

    fn check_file(
        path: &Path,
        rel: String,
        expected: &BTreeMap<String, ObjectId>,
        ignored: bool,
    ) -> io::Result<CheckedFile> {
        if !ignored && !expected.contains_key(&rel) {
            return Err(io::Error::other(format!(
                "workspace kept; unstored file: {rel}"
            )));
        }
        let mut options = OpenOptions::new();
        options.custom_flags(OPEN_REPARSE_POINT);
        if ignored {
            options
                .access_mode(DELETE)
                .share_mode(SHARE_READ | SHARE_WRITE);
        } else {
            // Windows refuses this open if any writer or writable mapping
            // exists, and refuses new writers while this handle remains open.
            options
                .access_mode(0x8000_0000 | DELETE)
                .share_mode(SHARE_READ);
        }
        let mut file = options.open(path).map_err(|err| file_error(&rel, err))?;
        let meta = file.metadata().map_err(|err| file_error(&rel, err))?;
        if !meta.is_file() || meta.file_attributes() & REPARSE_POINT != 0 {
            return Err(io::Error::other(format!(
                "workspace kept; not a plain file: {rel}"
            )));
        }
        if !ignored {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|err| file_error(&rel, err))?;
            if expected.get(&rel) != Some(&ObjectId::hash_object("blob", &bytes)) {
                return Err(io::Error::other(format!(
                    "workspace kept; unstored file: {rel}"
                )));
            }
        }
        Ok(CheckedFile { rel, file })
    }

    fn delete_checked(entry: CheckedFile) -> io::Result<()> {
        // This uses the same handle that denied writes and renames during
        // hashing. No second open can introduce a sharing or pathname race.
        rosen_platform::delete_on_close(&entry.file).map_err(|err| file_error(&entry.rel, err))?;
        drop(entry.file);
        Ok(())
    }
    fn relative(root: &Path, path: &Path) -> io::Result<String> {
        let rel = path.strip_prefix(root).map_err(io::Error::other)?;
        rel.to_str()
            .map(|rel| rel.replace('\\', "/"))
            .ok_or_else(|| io::Error::other("workspace kept; non-UTF-8 file name"))
    }

    fn named(root: &Path, path: &Path, err: io::Error) -> io::Error {
        file_error(
            &path.strip_prefix(root).unwrap_or(path).to_string_lossy(),
            err,
        )
    }

    fn file_error(rel: &str, err: io::Error) -> io::Error {
        io::Error::new(err.kind(), format!("workspace kept; {rel}: {err}"))
    }
}
