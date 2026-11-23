//! Final content verification before a workspace is deleted (SPEC.md R4).

use super::ignore::{always_ignored, is_ignore_file, IgnoreRules};
use super::index::Index;
use super::{scan, upload, SnapshotError, Snapshotter, WorkspaceMarker};
use rosen_store::ObjectStore;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

    /// Names files added, changed or removed on disk since the newest
    /// captured snapshot: the stored one, or the last offline journal record
    /// when records wait. With [`Snapshotter::unuploaded_paths`] it names
    /// everything the server does not hold yet (`rosen status`, `rosen diff`,
    /// R10). Like [`Snapshotter::unstored_paths`] it rereads every file and
    /// changes nothing; call it on a freshly opened snapshotter.
    ///
    /// When [`Snapshotter::defer_unreadable`] is set, a held or busy file is
    /// included in the returned paths instead of failing the scan.
    pub fn uncaptured_paths(&mut self) -> Result<Vec<String>, SnapshotError> {
        Ok(self
            .scan_differences(self.defer_unreadable)?
            .into_keys()
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
        // Deletion and rebase must see every file's content; a held or busy
        // one fails them.
        self.scan_differences(false)
    }

    /// Each path where the disk differs from the index (or, with
    /// `defer_unreadable`, could not be read because another process holds
    /// it), and whether a file exists on disk at it. The index is left as it
    /// was.
    fn scan_differences(
        &mut self,
        defer_unreadable: bool,
    ) -> Result<BTreeMap<String, bool>, SnapshotError> {
        let mut scanned = Index {
            files: self.index.files.clone(),
            gitlinks: self.index.gitlinks.clone(),
        };
        let candidates = relevant_files(
            &self.root,
            &self.index,
            &self.marker.ignore,
            &self.stored_ignores,
        )?;
        scanned
            .files
            .retain(|path, _| !self.sparse.includes(path.as_bytes()) || candidates.contains(path));
        // Read one file at a time so verification does not retain a second
        // in-memory copy of the entire workspace's contents.
        let mut unreadable = BTreeSet::new();
        for path in &candidates {
            let outcome = upload::read_candidates(
                &self.root,
                &mut scanned,
                &BTreeSet::from([path.clone()]),
                &BTreeSet::new(),
                &upload::Fallback::new(),
                defer_unreadable,
            )?;
            unreadable.extend(outcome.unreadable);
        }
        let paths: BTreeSet<_> = self
            .index
            .files
            .keys()
            .chain(scanned.files.keys())
            .chain(unreadable.iter())
            .collect();
        Ok(paths
            .into_iter()
            .filter(|path| {
                unreadable.contains(*path)
                    || file_identity(&self.index, path) != file_identity(&scanned, path)
            })
            .map(|path| {
                let on_disk = unreadable.contains(path) || scanned.files.contains_key(path);
                (path.clone(), on_disk)
            })
            .collect())
    }
}

fn file_identity(
    index: &Index,
    path: &str,
) -> Option<(rosen_core::FileMode, rosen_core::ObjectId)> {
    index.files.get(path).map(|entry| (entry.mode, entry.blob))
}

fn relevant_files(
    root: &Path,
    index: &Index,
    project_ignore: &[String],
    stored_ignores: &HashMap<String, Vec<u8>>,
) -> Result<BTreeSet<String>, SnapshotError> {
    // Ignored output never blocks deletion (SPEC R2, R4).
    let mut ignore =
        IgnoreRules::with_stored(root.to_path_buf(), project_ignore, stored_ignores.clone());
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
            if kind.is_dir() {
                check_nested_workspace(&entry.path())?;
                if scan::walks_into(&rel, &mut ignore, index) {
                    pending.push(rel);
                }
            } else if is_dir_link(&kind) && !index.tracks(&rel) && ignore.is_ignored(&rel, true) {
                continue;
            } else if scan::keeps_file(&rel, &mut ignore, index) {
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

#[cfg(windows)]
fn is_dir_link(kind: &fs::FileType) -> bool {
    use std::os::windows::fs::FileTypeExt;
    kind.is_symlink_dir()
}

#[cfg(not(windows))]
fn is_dir_link(_kind: &fs::FileType) -> bool {
    false
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
    /// Loads tracked `.gitignore` blobs from `store` as fallbacks for staged
    /// deletion, where an interrupted pass may have removed `.gitignore` on
    /// disk before reaching ignored build output (#430).
    pub(crate) fn load_stored_ignores(&mut self, store: &dyn ObjectStore) {
        self.stored_ignores.clear();
        for (path, entry) in &self.index.files {
            if !is_ignore_file(path) {
                continue;
            }
            let dir = path.strip_suffix("/.gitignore").unwrap_or("");
            if let Ok(blob) = store.read_blob(&entry.blob) {
                self.stored_ignores.insert(dir.to_owned(), blob.data);
            }
        }
    }

    /// Rechecks held bytes before platform-specific verified removal. The
    /// path-based scan above is advisory; Windows also excludes writers.
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
        let mut ignore = IgnoreRules::with_stored(
            self.root.clone(),
            &self.marker.ignore,
            self.stored_ignores.clone(),
        );
        crate::workspace::delete::remove_verified(&self.root, &expected, |rel, is_dir| {
            if is_dir {
                !self.index.tracks(rel) && !scan::walks_into(rel, &mut ignore, &self.index)
            } else {
                always_ignored(rel)
                    || (!self.index.tracks_under(rel)
                        && !scan::keeps_file(rel, &mut ignore, &self.index))
            }
        })
        .map_err(|err| SnapshotError::io(&self.root, err))
    }

    /// Closes a workspace whose directory deletion removed, along with its
    /// lease files outside it. Keeps them unless this snapshotter held the
    /// workspace for deletion.
    pub(crate) fn close_deleted(self) {
        self.journal.remove_leases();
    }
}
