//! A tree flattened into the directories and files a workspace holds.

use std::path::{Path, PathBuf};

use rosen_core::{FileMode, ObjectId};
use rosen_store::ObjectStore;

use super::{MaterialiseError, Sparse};

/// How a file is written to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Executable,
    Symlink,
}

/// One file of a tree, at its `/`-separated path from the tree's root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: Vec<u8>,
    pub blob: ObjectId,
    pub kind: FileKind,
}

/// Every directory and file a workspace holds for one tree. Directories come
/// parents first, so creating them in order never needs `create_dir_all`.
#[derive(Debug, Default)]
pub struct Layout {
    pub dirs: Vec<Vec<u8>>,
    pub files: Vec<FileEntry>,
}

impl Layout {
    /// Walks `tree` in the store, keeping only what `sparse` includes.
    pub fn of_tree(
        store: &dyn ObjectStore,
        tree: ObjectId,
        sparse: &Sparse,
    ) -> Result<Self, MaterialiseError> {
        let mut layout = Layout::default();
        let mut pending = vec![(Vec::new(), tree)];
        while let Some((dir, tree)) = pending.pop() {
            if is_empty_tree(&tree) {
                continue;
            }
            for entry in store.read_tree(&tree)?.entries() {
                check_component(&entry.name)?;
                let path = join(&dir, &entry.name);
                layout.add(path, entry.mode, entry.id, sparse, &mut pending);
            }
        }
        Ok(layout)
    }

    fn add(
        &mut self,
        path: Vec<u8>,
        mode: FileMode,
        id: ObjectId,
        sparse: &Sparse,
        pending: &mut Vec<(Vec<u8>, ObjectId)>,
    ) {
        let kind = match mode {
            _ if mode.is_tree() => {
                if sparse.enters(&path) {
                    self.dirs.push(path.clone());
                    pending.push((path, id));
                }
                return;
            }
            // Git checks a submodule out as an empty directory, and so does
            // Rosen: the gitlink is kept in the tree, its contents are not.
            FileMode::Submodule => {
                if sparse.includes(&path) {
                    self.dirs.push(path);
                }
                return;
            }
            FileMode::Executable => FileKind::Executable,
            FileMode::Symlink => FileKind::Symlink,
            _ => FileKind::Regular,
        };
        if sparse.includes(&path) {
            self.files.push(FileEntry {
                path,
                blob: id,
                kind,
            });
        }
    }
}

/// The empty tree is never read from the store: a change on an empty base
/// has it as its head, and nothing ever had to store it.
fn is_empty_tree(id: &ObjectId) -> bool {
    *id == ObjectId::hash_object("tree", b"")
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(dir.len() + 1 + name.len());
    if !dir.is_empty() {
        path.extend_from_slice(dir);
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// The on-disk path of a `/`-separated tree path under `root`.
pub fn disk_path(root: &Path, path: &[u8]) -> PathBuf {
    let mut full = root.to_path_buf();
    for component in path.split(|&byte| byte == b'/') {
        full.push(component_os_str(component));
    }
    full
}

#[cfg(unix)]
fn component_os_str(component: &[u8]) -> &std::ffi::OsStr {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(component)
}

/// Non-UTF-8 names were already refused by [`check_component`].
#[cfg(not(unix))]
fn component_os_str(component: &[u8]) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(std::str::from_utf8(component).unwrap_or("_"))
}

/// Refuses a tree entry name that this OS would not store as one plain path
/// component. Tree names already exclude `/`, `.`, `..` and `.git`; on Windows
/// a `\` or `:` would also let a tree write outside the workspace or into an
/// alternate data stream, so those names fail before anything is written.
fn check_component(name: &[u8]) -> Result<(), MaterialiseError> {
    if cfg!(windows) && !is_portable_windows_name(name) {
        return Err(MaterialiseError::UnsupportedName(
            String::from_utf8_lossy(name).into_owned(),
        ));
    }
    Ok(())
}

/// Windows refuses or silently rewrites these names: control and reserved
/// characters, a trailing dot or space, DOS device names, and `git~1`, the
/// short name that can alias `.git`.
fn is_portable_windows_name(name: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(name) else {
        return false;
    };
    let bad_char = |c: char| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*');
    if text.chars().any(bad_char) || text.ends_with(['.', ' ']) {
        return false;
    }
    let stem = text.split('.').next().unwrap_or(text).to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    !device && !text.eq_ignore_ascii_case("git~1")
}
