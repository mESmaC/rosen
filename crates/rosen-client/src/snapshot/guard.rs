//! Final content verification before a workspace is deleted (SPEC.md R4).

use super::ignore::{always_ignored, IgnoreRules};
use super::index::Index;
use super::{upload, SnapshotError, Snapshotter, WorkspaceMarker};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

impl Snapshotter {
    /// Names files added, changed or removed since the stored snapshot.
    ///
    /// This blocking scan rereads every relevant file, even when its size
    /// and timestamp have not changed. Call on a freshly opened snapshotter:
    /// an index advanced by a failed snapshot is not evidence of durability.
    /// The snapshot index and workspace marker remain unchanged.
    pub fn unstored_paths(&mut self) -> Result<Vec<String>, SnapshotError> {
        Ok(self.differences()?.into_keys().collect())
    }

    /// Like [`Snapshotter::unstored_paths`], but leaves out stored files
    /// that are missing from disk. Only for a workspace staged for deletion,
    /// where an interrupted verified removal may already have deleted them.
    pub(crate) fn unstored_paths_on_disk(&mut self) -> Result<Vec<String>, SnapshotError> {
        let differences = self.differences()?;
        Ok(differences
            .into_iter()
            .filter_map(|(path, on_disk)| on_disk.then_some(path))
            .collect())
    }

    /// Each differing path, and whether a file exists on disk at it.
    fn differences(&mut self) -> Result<BTreeMap<String, bool>, SnapshotError> {
        if self.index.build_trees()?.root != self.marker.tree {
            return Err(invalid(
                &self.root,
                "reopen the stored snapshot before deletion",
            ));
        }
        let mut scanned = Index {
            files: self.index.files.clone(),
            gitlinks: self.index.gitlinks.clone(),
        };
        let candidates = relevant_files(&self.root, &self.index)?;
        scanned
            .files
            .retain(|path, _| !self.sparse.includes(path.as_bytes()) || candidates.contains(path));
        // Read one file at a time so verification does not retain a second
        // in-memory copy of the entire workspace's contents.
        for path in &candidates {
            upload::read_candidates(
                &self.root,
                &mut scanned,
                &BTreeSet::from([path.clone()]),
                &BTreeSet::new(),
            )?;
        }
        let paths: BTreeSet<_> = self
            .index
            .files
            .keys()
            .chain(scanned.files.keys())
            .collect();
        Ok(paths
            .into_iter()
            .filter(|path| file_identity(&self.index, path) != file_identity(&scanned, path))
            .map(|path| (path.clone(), scanned.files.contains_key(path)))
            .collect())
    }
}

fn file_identity(
    index: &Index,
    path: &str,
) -> Option<(rosen_core::FileMode, rosen_core::ObjectId)> {
    index.files.get(path).map(|entry| (entry.mode, entry.blob))
}

fn relevant_files(root: &Path, index: &Index) -> Result<BTreeSet<String>, SnapshotError> {
    let mut ignore = IgnoreRules::new(root.to_path_buf());
    let mut files = BTreeSet::new();
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        let disk = root.join(&dir);
        for entry in fs::read_dir(&disk).map_err(|err| SnapshotError::io(&disk, err))? {
            let entry = entry.map_err(|err| SnapshotError::io(&disk, err))?;
            let name = entry.file_name().into_string().map_err(|_| {
                invalid(
                    &entry.path(),
                    "cannot verify a non-UTF-8 file name for deletion",
                )
            })?;
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if always_ignored(&rel) || index.in_gitlink(&rel) {
                continue;
            }
            let kind = entry
                .file_type()
                .map_err(|err| SnapshotError::io(&entry.path(), err))?;
            let ignored = ignore.is_ignored(&rel, kind.is_dir());
            if kind.is_dir() {
                check_nested_workspace(&entry.path())?;
                if !ignored || index.tracks_under(&rel) {
                    pending.push(rel);
                }
            } else if index.tracks(&rel) || !ignored {
                if !kind.is_file() && !kind.is_symlink() {
                    return Err(invalid(
                        &entry.path(),
                        "cannot verify a special file for deletion",
                    ));
                }
                files.insert(rel);
            }
        }
    }
    Ok(files)
}

fn check_nested_workspace(dir: &Path) -> Result<(), SnapshotError> {
    match fs::symlink_metadata(WorkspaceMarker::path(dir)) {
        Ok(_) => Err(invalid(
            dir,
            "deletion would remove another Rosen workspace",
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(SnapshotError::io(dir, err)),
    }
}

fn invalid(path: &Path, reason: &str) -> SnapshotError {
    SnapshotError::io(path, io::Error::new(io::ErrorKind::InvalidData, reason))
}

impl Snapshotter {
    /// Files whose versions remain only in journal records not yet stored
    /// on the server. Call after [`Snapshotter::hold_for_deletion`].
    pub fn unuploaded_paths(&self) -> Result<Vec<String>, SnapshotError> {
        let cache = super::journal::cache(&self.root)?;
        let mut before = Index::from_tree(&cache, self.marker.tree)?;
        let mut paths = BTreeSet::new();
        let journal_io = |err| SnapshotError::io(&self.root, err);
        for id in self.journal.pending().map_err(journal_io)? {
            let after = Index::from_tree(&cache, self.journal.root_of(id).map_err(journal_io)?)?;
            for path in before.files.keys().chain(after.files.keys()) {
                if file_identity(&before, path) != file_identity(&after, path) {
                    paths.insert(path.clone());
                }
            }
            // Named snapshots can hold only a message. Name their durable
            // record too, so an empty diff cannot hide an unuploaded entry.
            paths.insert(format!(".rosen/journal/{id}.json"));
            before = after;
        }
        Ok(paths.into_iter().collect())
    }
}

impl Snapshotter {
    /// Deletes only objects verified through write-excluded handles. The
    /// path-based scan above is advisory; this phase rechecks held bytes.
    pub(crate) fn remove_verified(&mut self) -> Result<(), SnapshotError> {
        if !self.index.gitlinks.is_empty() {
            return Err(invalid(&self.root, "workspace contains submodules"));
        }
        let expected = self
            .index
            .files
            .iter()
            .map(|(path, entry)| (path.clone(), entry.blob))
            .collect();
        let mut ignore = IgnoreRules::new(self.root.clone());
        crate::workspace::delete::remove_verified(&self.root, &expected, |rel, is_dir| {
            always_ignored(rel)
                || (!self.index.tracks(rel)
                    && !self.index.tracks_under(rel)
                    && ignore.is_ignored(rel, is_dir))
        })
        .map_err(|err| SnapshotError::io(&self.root, err))
    }
}
