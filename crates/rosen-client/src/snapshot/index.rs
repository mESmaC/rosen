//! The client's view of a workspace's tracked files: path, mode and blob of
//! each, as last stored or read, and the trees they make.

use rosen_core::{FileMode, Object, ObjectId, Tree, TreeEntry};
use rosen_store::ObjectStore;
use std::collections::BTreeMap;
use std::time::SystemTime;

use super::SnapshotError;

/// What a file looked like on disk when its blob was read. A full rescan
/// rereads a file only when this no longer matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub len: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: FileMode,
    pub blob: ObjectId,
    /// `None` until read from disk, or when the read was too close to the
    /// file's last write to trust its timestamp.
    pub stat: Option<Stat>,
}

#[derive(Debug, Default)]
pub struct Index {
    /// Every tracked file, by `/`-separated path from the workspace root.
    pub files: BTreeMap<String, Entry>,
    /// Gitlinks (submodules) are kept as the tree holds them. Their
    /// directories are never scanned.
    pub gitlinks: BTreeMap<String, ObjectId>,
}

/// Trees built from an index: the root, and every tree with its body,
/// children before parents, so they can be uploaded in this order.
pub struct BuiltTrees {
    pub root: ObjectId,
    pub trees: Vec<(ObjectId, Vec<u8>)>,
}

impl Index {
    /// The index of a stored tree, with no disk stats yet.
    pub fn from_tree(store: &dyn ObjectStore, tree: ObjectId) -> Result<Self, SnapshotError> {
        let mut index = Index::default();
        let mut pending = vec![(String::new(), tree)];
        while let Some((dir, tree)) = pending.pop() {
            if tree == empty_tree() {
                continue;
            }
            for entry in store.read_tree(&tree)?.entries() {
                let name = std::str::from_utf8(&entry.name)
                    .map_err(|_| SnapshotError::NonUtf8Path(entry.name.clone()))?;
                let path = join(&dir, name);
                match entry.mode {
                    mode if mode.is_tree() => pending.push((path, entry.id)),
                    FileMode::Submodule => {
                        index.gitlinks.insert(path, entry.id);
                    }
                    mode => {
                        let file = Entry {
                            mode,
                            blob: entry.id,
                            stat: None,
                        };
                        index.files.insert(path, file);
                    }
                }
            }
        }
        Ok(index)
    }

    pub fn tracks(&self, rel: &str) -> bool {
        self.files.contains_key(rel) || self.gitlinks.contains_key(rel)
    }

    /// Whether any tracked file lies under the directory `rel`.
    pub fn tracks_under(&self, rel: &str) -> bool {
        let prefix = format!("{rel}/");
        self.files
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(path, _)| path.starts_with(&prefix))
    }

    /// Whether `rel` is a gitlink or lies inside one.
    pub fn in_gitlink(&self, rel: &str) -> bool {
        self.gitlinks
            .keys()
            .any(|link| rel == link || rel.starts_with(&format!("{link}/")))
    }

    /// Tracked file paths at or under `rel` (every file for `""`).
    pub fn paths_under(&self, rel: &str) -> Vec<String> {
        if rel.is_empty() {
            return self.files.keys().cloned().collect();
        }
        let prefix = format!("{rel}/");
        let mut paths: Vec<String> = self
            .files
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(path, _)| path.clone())
            .collect();
        if self.files.contains_key(rel) {
            paths.push(rel.to_owned());
        }
        paths
    }

    /// Builds every tree of the index in git format.
    pub fn build_trees(&self) -> Result<BuiltTrees, SnapshotError> {
        let mut root = Dir::default();
        for (path, entry) in &self.files {
            root.insert(path, entry.mode, entry.blob);
        }
        for (path, id) in &self.gitlinks {
            root.insert(path, FileMode::Submodule, *id);
        }
        let mut trees = Vec::new();
        let root = root.build(&mut trees)?;
        Ok(BuiltTrees { root, trees })
    }
}

/// One directory while building trees.
#[derive(Default)]
struct Dir {
    files: Vec<(String, FileMode, ObjectId)>,
    dirs: BTreeMap<String, Dir>,
}

impl Dir {
    fn insert(&mut self, path: &str, mode: FileMode, id: ObjectId) {
        match path.split_once('/') {
            Some((head, rest)) => self
                .dirs
                .entry(head.to_owned())
                .or_default()
                .insert(rest, mode, id),
            None => self.files.push((path.to_owned(), mode, id)),
        }
    }

    fn build(self, out: &mut Vec<(ObjectId, Vec<u8>)>) -> Result<ObjectId, SnapshotError> {
        let mut entries = Vec::with_capacity(self.files.len() + self.dirs.len());
        for (name, dir) in self.dirs {
            let id = dir.build(out)?;
            entries.push(TreeEntry::new(FileMode::Directory, name, id)?);
        }
        for (name, mode, id) in self.files {
            entries.push(TreeEntry::new(mode, name, id)?);
        }
        let body = Object::Tree(Tree::new(entries)?).encode_body();
        let id = ObjectId::hash_object("tree", &body);
        out.push((id, body));
        Ok(id)
    }
}

pub fn empty_tree() -> ObjectId {
    ObjectId::hash_object("tree", b"")
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}
