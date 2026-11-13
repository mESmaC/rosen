//! A tree flattened into the directories and files a workspace holds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rosen_core::{FileMode, ObjectId, TreeEntry};
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
        layout.walk(store, vec![(Vec::new(), tree)], sparse)?;
        Ok(layout)
    }

    /// The whole-tree layouts of `old` and `new`, restricted to the parts
    /// that differ: `.0` holds what `old` has there and `.1` what `new` has.
    /// Directories present in both with different contents are listed in
    /// both. Subtrees with equal IDs are never read, so the cost follows the
    /// size of the change rather than of the tree; a ready workspace behind
    /// the base by a few lands catches up in milliseconds (R3).
    pub fn diff(
        store: &dyn ObjectStore,
        old: ObjectId,
        new: ObjectId,
    ) -> Result<(Self, Self), MaterialiseError> {
        let all = Sparse::all();
        let (mut before, mut after) = (Layout::default(), Layout::default());
        let (mut only_old, mut only_new) = (Vec::new(), Vec::new());
        let mut pending = vec![(Vec::new(), old, new)];
        while let Some((dir, old, new)) = pending.pop() {
            if old == new {
                continue;
            }
            let (old_tree, new_tree) = (read_entries(store, old)?, read_entries(store, new)?);
            let mut names: BTreeMap<&[u8], (Option<&TreeEntry>, Option<&TreeEntry>)> =
                BTreeMap::new();
            for entry in &old_tree {
                names.entry(&entry.name).or_default().0 = Some(entry);
            }
            for entry in &new_tree {
                names.entry(&entry.name).or_default().1 = Some(entry);
            }
            for (name, (was, now)) in names {
                if was == now {
                    continue;
                }
                check_component(name)?;
                let path = join(&dir, name);
                match (was, now) {
                    (Some(a), Some(b)) if a.mode.is_tree() && b.mode.is_tree() => {
                        before.dirs.push(path.clone());
                        after.dirs.push(path.clone());
                        pending.push((path, a.id, b.id));
                    }
                    _ => {
                        if let Some(a) = was {
                            before.add(path.clone(), a.mode, a.id, &all, &mut only_old);
                        }
                        if let Some(b) = now {
                            after.add(path, b.mode, b.id, &all, &mut only_new);
                        }
                    }
                }
            }
        }
        before.walk(store, only_old, &all)?;
        after.walk(store, only_new, &all)?;
        Ok((before, after))
    }

    /// Adds every directory and file under each pending `(path, tree)`.
    fn walk(
        &mut self,
        store: &dyn ObjectStore,
        mut pending: Vec<(Vec<u8>, ObjectId)>,
        sparse: &Sparse,
    ) -> Result<(), MaterialiseError> {
        while let Some((dir, tree)) = pending.pop() {
            for entry in read_entries(store, tree)? {
                check_component(&entry.name)?;
                let path = join(&dir, &entry.name);
                self.add(path, entry.mode, entry.id, sparse, &mut pending);
            }
        }
        Ok(())
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

fn read_entries(
    store: &dyn ObjectStore,
    tree: ObjectId,
) -> Result<Vec<TreeEntry>, MaterialiseError> {
    if is_empty_tree(&tree) {
        return Ok(Vec::new());
    }
    Ok(store.read_tree(&tree)?.entries().to_vec())
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
