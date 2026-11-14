//! Finding which tracked or new files may differ from the index: a walk of
//! the whole workspace (or one directory), or a check of the paths the
//! watcher reported.

use super::ignore::{always_ignored, is_ignore_file, IgnoreRules};
use super::index::{Index, Stat};
use crate::workspace::Sparse;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

/// What a scan found. Removed files are already gone from the index;
/// candidates are read and hashed before the snapshot.
#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub candidates: BTreeSet<String>,
    pub removed: usize,
    /// A `.gitignore` changed, so every untracked path must be checked again.
    pub ignore_changed: bool,
}

/// Walks the directory `rel_dir` (`""` for the whole workspace). Each file
/// not in the index, or whose size or time differs from the one recorded,
/// becomes a candidate; each indexed file under `rel_dir` that is no longer
/// on disk is removed.
pub fn scan_dir(view: &mut View<'_>, rel_dir: &str, out: &mut ScanOutcome) -> io::Result<()> {
    let View {
        root,
        ignore,
        index,
        sparse,
    } = view;
    let mut seen = BTreeSet::new();
    let mut pending = vec![rel_dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(disk(root, &dir)) {
            Ok(entries) => entries,
            // Removed while walking: whatever it held is gone.
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        for entry in entries {
            let entry = entry?;
            // A name that is not UTF-8 cannot go in a tree Rosen writes.
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let rel = join(&dir, &name);
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if walks_into(&rel, ignore, index) {
                    pending.push(rel);
                }
            } else if keeps_file(&rel, ignore, index) {
                if stat_differs(index, &rel, &entry.metadata()?) {
                    out.candidates.insert(rel.clone());
                }
                seen.insert(rel);
            }
        }
    }
    for path in index.paths_under(rel_dir) {
        if !seen.contains(&path) && sparse.includes(path.as_bytes()) {
            index.files.remove(&path);
            out.removed += 1;
        }
    }
    Ok(())
}

/// Checks one path the watcher reported. A reported file is always a
/// candidate: its timestamp cannot tell two writes in one clock tick apart.
pub fn scan_path(view: &mut View<'_>, rel: &str, out: &mut ScanOutcome) -> io::Result<()> {
    if rel.is_empty() || always_ignored(rel) || view.index.in_gitlink(rel) {
        return Ok(());
    }
    if is_ignore_file(rel) {
        view.ignore.forget();
        out.ignore_changed = true;
    }
    match fs::symlink_metadata(disk(view.root, rel)) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            view.remove_under(rel, true, out);
            Ok(())
        }
        Err(err) => Err(err),
        Ok(meta) if meta.is_dir() => {
            // A file replaced by a directory of the same name.
            if view.index.files.remove(rel).is_some() {
                out.removed += 1;
            }
            if walks_into(rel, view.ignore, view.index) {
                scan_dir(view, rel, out)?;
            }
            Ok(())
        }
        Ok(_) => {
            // A directory replaced by a file of the same name.
            view.remove_under(rel, false, out);
            if keeps_file(rel, view.ignore, view.index) {
                out.candidates.insert(rel.to_owned());
            }
            Ok(())
        }
    }
}

/// Drops candidates the current ignore rules exclude. A batch can walk a
/// directory under its cached rules before it reaches that directory's new
/// `.gitignore`, so its candidates must be checked again once the rules are
/// fresh, or an ignored file is uploaded and then stays tracked (SPEC R2).
pub fn drop_ignored(ignore: &mut IgnoreRules, index: &Index, out: &mut ScanOutcome) {
    out.candidates.retain(|rel| keeps_file(rel, ignore, index));
}

/// What a scan reads and updates.
pub struct View<'a> {
    pub root: &'a Path,
    pub ignore: &'a mut IgnoreRules,
    pub index: &'a mut Index,
    /// Tracked files outside it are not on disk by design, so their absence
    /// never counts as a deletion.
    pub sparse: &'a Sparse,
}

impl View<'_> {
    /// Removes tracked files under `rel` that the workspace holds, and `rel`
    /// itself when `itself` is set.
    fn remove_under(&mut self, rel: &str, itself: bool, out: &mut ScanOutcome) {
        for path in self.index.paths_under(rel) {
            if (itself || path != rel) && self.sparse.includes(path.as_bytes()) {
                self.index.files.remove(&path);
                out.removed += 1;
            }
        }
    }
}

/// An ignored directory is still walked when it holds tracked files, but
/// only those files are kept from it.
fn walks_into(rel: &str, ignore: &mut IgnoreRules, index: &Index) -> bool {
    if always_ignored(rel) || index.in_gitlink(rel) {
        return false;
    }
    !ignore.is_ignored(rel, true) || index.tracks_under(rel)
}

fn keeps_file(rel: &str, ignore: &mut IgnoreRules, index: &Index) -> bool {
    !always_ignored(rel) && (index.tracks(rel) || !ignore.is_ignored(rel, false))
}

fn stat_differs(index: &Index, rel: &str, meta: &fs::Metadata) -> bool {
    let Some(recorded) = index.files.get(rel).and_then(|entry| entry.stat) else {
        return true;
    };
    stat_of(meta) != Some(recorded)
}

pub fn stat_of(meta: &fs::Metadata) -> Option<Stat> {
    Some(Stat {
        len: meta.len(),
        modified: meta.modified().ok()?,
    })
}

pub fn disk(root: &Path, rel: &str) -> std::path::PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}
