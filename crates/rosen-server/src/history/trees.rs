//! Tree walks for history queries: which files differ between two trees,
//! what a path names in a tree, and writing a tree with some files replaced.
//! Only subtrees whose IDs differ are opened, so comparing two versions of
//! a large project reads only the directories that changed.

use super::{HistoryError, HistoryResult};
use rosen_core::{FileMode, ObjectId, Tree, TreeEntry};
use rosen_protocol::{DirEntry, ReadAnswer};
use rosen_store::ObjectStore;
use std::collections::BTreeMap;

/// A file-like tree entry: a file, executable, symlink or gitlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub mode: FileMode,
    pub id: ObjectId,
}

impl Entry {
    /// Whether the entry's id names a blob this server holds. A gitlink
    /// names a commit in another repository.
    pub fn is_blob(self) -> bool {
        self.mode != FileMode::Submodule
    }
}

/// One file that differs between two trees. `None` is absent on that side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub old: Option<Entry>,
    pub new: Option<Entry>,
}

/// Normalises a client path: `/` separators, no leading `./` or `/`, no
/// trailing `/`. Empty means the root.
pub fn normalise(path: &str) -> HistoryResult<String> {
    let path = path.replace('\\', "/");
    let segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if segments.contains(&"..") {
        return Err(HistoryError::Invalid(format!(
            "{path:?} leaves the repository"
        )));
    }
    Ok(segments.join("/"))
}

/// Whether `path` is one of `filters` or lies under one. No filters keeps
/// every path.
pub fn selected(path: &str, filters: &[String]) -> bool {
    filters.is_empty() || filters.iter().any(|f| f.is_empty() || within(path, f))
}

fn within(path: &str, dir: &str) -> bool {
    path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

/// Whether a directory can hold a selected path, so it is worth opening.
fn may_hold(dir: &str, filters: &[String]) -> bool {
    filters.is_empty() || filters.iter().any(|f| within(dir, f) || within(f, dir))
}

/// Every file that differs between `old` and `new`, in path order, limited
/// to `filters`. A path that is a file on one side and a directory on the
/// other shows as the file removed (or added) plus each file under the
/// directory.
pub fn changed_files(
    store: &dyn ObjectStore,
    old: Option<ObjectId>,
    new: Option<ObjectId>,
    filters: &[String],
) -> HistoryResult<Vec<Change>> {
    let mut out = Vec::new();
    diff_dirs(store, "", old, new, filters, &mut out)?;
    Ok(out)
}

fn diff_dirs(
    store: &dyn ObjectStore,
    prefix: &str,
    old: Option<ObjectId>,
    new: Option<ObjectId>,
    filters: &[String],
    out: &mut Vec<Change>,
) -> HistoryResult<()> {
    if old == new || !may_hold(prefix, filters) {
        return Ok(());
    }
    let mut names: BTreeMap<Vec<u8>, [Option<TreeEntry>; 2]> = BTreeMap::new();
    for (side, tree) in [old, new].into_iter().enumerate() {
        let Some(tree) = tree else { continue };
        for entry in store.read_tree(&tree)?.entries() {
            names.entry(entry.name.clone()).or_default()[side] = Some(entry.clone());
        }
    }
    for (name, [before, after]) in names {
        if before == after {
            continue;
        }
        let name = String::from_utf8_lossy(&name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let dir = |e: &Option<TreeEntry>| e.as_ref().filter(|e| e.mode.is_tree()).map(|e| e.id);
        let file = |e: &Option<TreeEntry>| {
            e.as_ref().filter(|e| !e.mode.is_tree()).map(|e| Entry {
                mode: e.mode,
                id: e.id,
            })
        };
        let (old_file, new_file) = (file(&before), file(&after));
        if old_file != new_file && selected(&path, filters) {
            out.push(Change {
                path: path.clone(),
                old: old_file,
                new: new_file,
            });
        }
        diff_dirs(store, &path, dir(&before), dir(&after), filters, out)?;
    }
    Ok(())
}

/// What `path` names in `tree`: `Ok(None)` when nothing is there.
pub fn entry_at(
    store: &dyn ObjectStore,
    tree: ObjectId,
    path: &str,
) -> HistoryResult<Option<TreeEntry>> {
    let mut current = tree;
    let mut segments = path.split('/').filter(|s| !s.is_empty()).peekable();
    while let Some(segment) = segments.next() {
        let listing = store.read_tree(&current)?;
        let Some(entry) = listing.get(segment.as_bytes()) else {
            return Ok(None);
        };
        if segments.peek().is_none() {
            return Ok(Some(entry.clone()));
        }
        if !entry.mode.is_tree() {
            return Ok(None);
        }
        current = entry.id;
    }
    Ok(None)
}

/// The file at `path`, if `tree` holds a file there.
pub fn file_at(
    store: &dyn ObjectStore,
    tree: ObjectId,
    path: &str,
) -> HistoryResult<Option<Entry>> {
    Ok(entry_at(store, tree, path)?
        .filter(|e| !e.mode.is_tree())
        .map(|e| Entry {
            mode: e.mode,
            id: e.id,
        }))
}

/// A file's bytes or a directory's listing (R11 file read).
pub fn read(store: &dyn ObjectStore, tree: ObjectId, path: &str) -> HistoryResult<ReadAnswer> {
    let path = normalise(path)?;
    let entry = if path.is_empty() {
        TreeEntry::new(FileMode::Directory, "root", tree)?
    } else {
        entry_at(store, tree, &path)?
            .ok_or_else(|| HistoryError::NotFound(format!("{path} does not exist in {tree}")))?
    };
    if entry.mode.is_tree() {
        let entries = store
            .read_tree(&entry.id)?
            .entries()
            .iter()
            .map(|e| DirEntry {
                name: String::from_utf8_lossy(&e.name).into_owned(),
                mode: e.mode.as_octal().into_owned(),
                id: e.id,
            })
            .collect();
        return Ok(ReadAnswer::Directory {
            id: entry.id,
            entries,
        });
    }
    if entry.mode == FileMode::Submodule {
        return Err(HistoryError::Invalid(format!(
            "{path} is a submodule at commit {}; its files live in another repository",
            entry.id
        )));
    }
    Ok(ReadAnswer::File {
        mode: entry.mode.as_octal().into_owned(),
        id: entry.id,
        data: store.read_blob(&entry.id)?.data,
    })
}

/// The bytes of a file entry; a gitlink reads as its commit ID.
pub fn content(store: &dyn ObjectStore, entry: Option<Entry>) -> HistoryResult<Option<Vec<u8>>> {
    match entry {
        None => Ok(None),
        Some(e) if !e.is_blob() => Ok(Some(e.id.to_hex().into_bytes())),
        Some(e) => Ok(Some(store.read_blob(&e.id)?.data)),
    }
}

/// Writes `tree` with each path in `edits` set to a file or removed, and
/// returns the new root (`None` if nothing is left). A path that would need
/// to be a file and a directory at once is reported in `collisions` and left
/// as it was.
pub fn apply(
    store: &dyn ObjectStore,
    tree: Option<ObjectId>,
    edits: &BTreeMap<String, Option<Entry>>,
    collisions: &mut Vec<String>,
) -> HistoryResult<Option<ObjectId>> {
    apply_in(store, "", tree, edits, collisions)
}

fn apply_in(
    store: &dyn ObjectStore,
    prefix: &str,
    tree: Option<ObjectId>,
    edits: &BTreeMap<String, Option<Entry>>,
    collisions: &mut Vec<String>,
) -> HistoryResult<Option<ObjectId>> {
    let mut entries: BTreeMap<Vec<u8>, TreeEntry> = match tree {
        Some(id) => store
            .read_tree(&id)?
            .entries()
            .iter()
            .map(|e| (e.name.clone(), e.clone()))
            .collect(),
        None => BTreeMap::new(),
    };
    // Removals here first, then subdirectories, then files set here, so a
    // file replacing a directory (or the reverse) finds the name free.
    let mut below: BTreeMap<&str, BTreeMap<String, Option<Entry>>> = BTreeMap::new();
    let mut sets = Vec::new();
    for (path, edit) in edits {
        match (path.split_once('/'), edit) {
            (None, None) => set_leaf(&mut entries, prefix, path, None, collisions)?,
            (None, Some(file)) => sets.push((path, *file)),
            (Some((dir, rest)), _) => {
                below.entry(dir).or_default().insert(rest.to_owned(), *edit);
            }
        }
    }
    apply_below(store, prefix, &mut entries, below, collisions)?;
    for (name, file) in sets {
        set_leaf(&mut entries, prefix, name, Some(file), collisions)?;
    }
    if entries.is_empty() {
        return Ok(None);
    }
    let tree = Tree::new(entries.into_values().collect())?;
    Ok(Some(store.put_tree(&tree)?))
}

fn apply_below(
    store: &dyn ObjectStore,
    prefix: &str,
    entries: &mut BTreeMap<Vec<u8>, TreeEntry>,
    below: BTreeMap<&str, BTreeMap<String, Option<Entry>>>,
    collisions: &mut Vec<String>,
) -> HistoryResult<()> {
    for (dir, sub_edits) in below {
        let path = join(prefix, dir);
        let existing = entries.get(dir.as_bytes());
        if existing.is_some_and(|e| !e.mode.is_tree()) {
            collisions.extend(sub_edits.keys().map(|rest| join(&path, rest)));
            continue;
        }
        match apply_in(store, &path, existing.map(|e| e.id), &sub_edits, collisions)? {
            Some(id) => {
                entries.insert(dir.into(), TreeEntry::new(FileMode::Directory, dir, id)?);
            }
            None => {
                entries.remove(dir.as_bytes());
            }
        }
    }
    Ok(())
}

fn set_leaf(
    entries: &mut BTreeMap<Vec<u8>, TreeEntry>,
    prefix: &str,
    name: &str,
    edit: Option<Entry>,
    collisions: &mut Vec<String>,
) -> HistoryResult<()> {
    let existing_dir = entries
        .get(name.as_bytes())
        .is_some_and(|e| e.mode.is_tree());
    match edit {
        // Removing a file never removes a directory of the same name.
        None if !existing_dir => {
            entries.remove(name.as_bytes());
        }
        None => {}
        Some(_) if existing_dir => collisions.push(join(prefix, name)),
        Some(file) => {
            entries.insert(name.into(), TreeEntry::new(file.mode, name, file.id)?);
        }
    }
    Ok(())
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}
