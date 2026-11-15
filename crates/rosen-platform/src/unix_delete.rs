//! Directory-relative operations for verified Unix workspace removal.
use rustix::fs::{self, AtFlags, Dir, Mode, OFlags, CWD};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub struct Directory(File);

impl Directory {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self(
            fs::openat(CWD, path, directory_flags(), Mode::empty())?.into(),
        ))
    }

    pub fn from_file(file: File) -> io::Result<Self> {
        if !file.metadata()?.is_dir() {
            return Err(io::Error::other("not a plain directory"));
        }
        Ok(Self(file))
    }

    pub fn file(&self) -> &File {
        &self.0
    }

    pub fn names(&self) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in Dir::read_from(&self.0)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn child(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        // NONBLOCK prevents a substituted FIFO from hanging cleanup.
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        Ok(fs::openat(&self.0, name, flags, Mode::empty())?.into())
    }

    /// Isolates the selected entry before checking its identity again.
    /// A statat/unlinkat pair alone can unlink an intervening replacement.
    /// On any refusal or interruption the isolated entry stays on disk.
    /// Directories use identity-checked, nonrecursive rmdir; `verify` is
    /// called only for files.
    pub fn remove_checked(
        &self,
        name: &OsStr,
        held: &File,
        verify: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        component(name)?;
        if held.metadata()?.is_dir() {
            return self.remove_directory(name, held);
        }
        let (slot_name, slot) = self.slot()?;
        let result = self.remove_in_slot(name, held, verify, &slot_name, &slot);
        // Removal is nonrecursive: a concurrent addition always survives.
        let cleanup = fs::unlinkat(&self.0, &slot_name, AtFlags::REMOVEDIR);
        result?;
        cleanup?;
        Ok(())
    }

    fn remove_directory(&self, name: &OsStr, held: &File) -> io::Result<()> {
        let current = fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let checked = fs::fstat(held)?;
        if (current.st_dev, current.st_ino) != (checked.st_dev, checked.st_ino) {
            return Err(io::Error::other("directory replaced during deletion"));
        }
        // Unlike unlinking a file, rmdir cannot remove bytes: even a
        // directory substituted after the check must be empty to go.
        fs::unlinkat(&self.0, name, AtFlags::REMOVEDIR)?;
        Ok(())
    }

    fn remove_in_slot(
        &self,
        name: &OsStr,
        held: &File,
        verify: impl FnOnce(&File) -> io::Result<()>,
        slot_name: &OsStr,
        slot: &Self,
    ) -> io::Result<()> {
        let result = (|| {
            fs::renameat(&self.0, name, &slot.0, "entry")?;
            let moved = fs::statat(&slot.0, "entry", AtFlags::SYMLINK_NOFOLLOW)?;
            let checked = fs::fstat(held)?;
            if (moved.st_dev, moved.st_ino) != (checked.st_dev, checked.st_ino) {
                return Err(io::Error::other("entry replaced during deletion"));
            }
            verify(held)?;
            fs::unlinkat(&slot.0, "entry", AtFlags::empty())?;
            Ok(())
        })();
        result.map_err(|err: io::Error| {
            io::Error::new(
                err.kind(),
                format!(
                    "{err}; retained entries, if any, in {}/entry",
                    slot_name.to_string_lossy()
                ),
            )
        })
    }

    fn slot(&self) -> io::Result<(OsString, Self)> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|err| io::Error::other(err.to_string()))?;
        let name = OsString::from(format!(
            ".rosen-remove-{:032x}",
            u128::from_ne_bytes(random)
        ));
        fs::mkdirat(&self.0, &name, Mode::RWXU)?;
        let file = fs::openat(&self.0, &name, directory_flags(), Mode::empty())?.into();
        Ok((name, Self(file)))
    }
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY
}

fn component(name: &OsStr) -> io::Result<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a child name",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // R4 risk: neither a stale verification handle nor a replacement made
    // after the last identity check may cause deletion of new user bytes.
    #[test]
    fn replacements_survive_before_and_after_isolation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = Directory::open(root).unwrap();
        std::fs::write(root.join("file"), b"stored").unwrap();
        let held = dir.child(OsStr::new("file")).unwrap();
        std::fs::rename(root.join("file"), root.join("original")).unwrap();
        std::fs::write(root.join("file"), b"replacement").unwrap();
        let error = dir
            .remove_checked(OsStr::new("file"), &held, |_| Ok(()))
            .unwrap_err();
        assert!(error.to_string().contains("replaced during deletion"));
        let slot = dir
            .names()
            .unwrap()
            .into_iter()
            .find(|name| name.to_string_lossy().starts_with(".rosen-remove-"))
            .unwrap();
        assert_eq!(
            std::fs::read(root.join(slot).join("entry")).unwrap(),
            b"replacement"
        );
        assert_eq!(std::fs::read(root.join("original")).unwrap(), b"stored");
        dir.remove_checked(OsStr::new("original"), &held, |_| {
            std::fs::write(root.join("original"), b"later replacement")
        })
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("original")).unwrap(),
            b"later replacement"
        );
    }
}
