//! Final content verification before a workspace is deleted (SPEC.md R4).

use super::ignore::{always_ignored, IgnoreRules};
use super::index::Index;
use super::{upload, SnapshotError, Snapshotter, WorkspaceMarker};
use std::collections::BTreeSet;
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
            .cloned()
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
