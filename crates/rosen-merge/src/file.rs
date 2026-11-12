//! File-level and tree-level three-way merge.
//!
//! A file is merged as a whole first (added, deleted, unchanged on one side)
//! and line by line only when both sides changed its content. Per SPEC R5,
//! generated files are never merged and registration lists keep both sides'
//! additions.

use std::collections::{BTreeMap, BTreeSet};

use crate::lines::split_lines;
use crate::merge::{merge_text, ConflictRegion, Hunk, MarkerLabels, TextMerge};

/// How a path merges, from project configuration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MergePolicy {
    #[default]
    Text,
    /// A generated file: when both sides change it, it is regenerated on the
    /// merge result instead of merged.
    Generated,
    /// A registration list: where both sides changed the same region, the
    /// result keeps ours, then the lines theirs added that ours lacks.
    RegistrationList,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// The outcome of merging one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileMerge {
    /// The merged content, or `None` when the merge deletes the path.
    Resolved(Option<Vec<u8>>),
    Conflicted(FileConflict),
    /// A generated file both sides changed. The caller regenerates it on the
    /// merge result; Rosen itself runs no generators (SPEC D9).
    Regenerate,
}

/// Why a path did not merge automatically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileConflict {
    /// Both sides edited the same lines differently.
    Content(TextMerge),
    /// One side deleted the file while the other changed it.
    ModifyDelete {
        deleted_by: Side,
        base: Vec<u8>,
        modified: Vec<u8>,
    },
    /// Both sides changed (or added) a binary file differently.
    Binary {
        base: Option<Vec<u8>>,
        ours: Vec<u8>,
        theirs: Vec<u8>,
    },
    /// The merge keeps this path as a file while another kept path needs it
    /// to be a directory.
    FileDirectory { content: Vec<u8> },
}

impl FileConflict {
    /// What a workspace shows for the path while the conflict is open.
    pub fn workspace_text(&self, labels: MarkerLabels) -> Vec<u8> {
        match self {
            FileConflict::Content(merge) => merge.render(labels),
            FileConflict::ModifyDelete { modified, .. } => modified.clone(),
            FileConflict::Binary { ours, .. } => ours.clone(),
            FileConflict::FileDirectory { content } => content.clone(),
        }
    }
}

/// Merges one path. `None` means the path is absent in that version.
pub fn merge_file(
    base: Option<&[u8]>,
    ours: Option<&[u8]>,
    theirs: Option<&[u8]>,
    policy: MergePolicy,
) -> FileMerge {
    if ours == theirs || theirs == base {
        return FileMerge::Resolved(ours.map(<[u8]>::to_vec));
    }
    if ours == base {
        return FileMerge::Resolved(theirs.map(<[u8]>::to_vec));
    }
    if policy == MergePolicy::Generated {
        return FileMerge::Regenerate;
    }
    let (Some(ours), Some(theirs)) = (ours, theirs) else {
        // Both sides changed the file and they differ, so a missing side
        // deleted a file the other modified (a base must exist: two sides
        // that both lack the file would be equal).
        let deleted_by = if ours.is_none() {
            Side::Ours
        } else {
            Side::Theirs
        };
        let modified = ours.or(theirs).unwrap_or_default().to_vec();
        let base = base.unwrap_or_default().to_vec();
        return FileMerge::Conflicted(FileConflict::ModifyDelete {
            deleted_by,
            base,
            modified,
        });
    };
    if [base.unwrap_or_default(), ours, theirs]
        .iter()
        .any(|text| is_binary(text))
    {
        let (base, ours, theirs) = (base.map(<[u8]>::to_vec), ours.to_vec(), theirs.to_vec());
        return FileMerge::Conflicted(FileConflict::Binary { base, ours, theirs });
    }
    // A file both sides added merges against an empty base, so identical
    // leading and trailing lines still merge and the rest conflicts.
    let mut merged = merge_text(base.unwrap_or_default(), ours, theirs);
    if policy == MergePolicy::RegistrationList {
        merged = keep_both_additions(merged);
    }
    match merged.clean_text() {
        Some(text) => FileMerge::Resolved(Some(text)),
        None => FileMerge::Conflicted(FileConflict::Content(merged)),
    }
}

/// Git's heuristic: a NUL byte in the first 8000 bytes marks a binary file.
fn is_binary(text: &[u8]) -> bool {
    text.iter().take(8000).any(|&byte| byte == 0)
}

fn keep_both_additions(merged: TextMerge) -> TextMerge {
    let hunks = merged.hunks.into_iter().map(|hunk| match hunk {
        Hunk::Conflict(region) => Hunk::Clean(union_of_additions(&region)),
        clean => clean,
    });
    TextMerge {
        hunks: hunks.collect(),
    }
}

/// Ours' lines that theirs did not delete, then the lines theirs added that
/// ours lacks. A base line either side deleted stays deleted, so the union
/// never brings back a removed entry (issue #19).
fn union_of_additions(region: &ConflictRegion) -> Vec<u8> {
    let [base, ours, theirs] =
        [&region.base, &region.ours, &region.theirs].map(|text| split_lines(text));
    let deleted_by_theirs = |line: &&[u8]| base.contains(line) && !theirs.contains(line);
    let kept_ours = ours.iter().filter(|line| !deleted_by_theirs(line));
    let added_by_theirs = theirs
        .iter()
        .filter(|line| !base.contains(line) && !ours.contains(line));
    let mut text = Vec::new();
    for line in kept_ours.chain(added_by_theirs) {
        // Keep each kept line terminated, so two entries never fuse.
        if text.last().is_some_and(|&byte| byte != b'\n') {
            text.push(b'\n');
        }
        text.extend_from_slice(line);
    }
    text
}

/// The result of merging every path of two trees against their merge base.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeMerge {
    /// Every path that exists in the merge result, with its content.
    pub files: BTreeMap<String, Vec<u8>>,
    pub conflicts: BTreeMap<String, FileConflict>,
    /// Generated paths both sides changed, to be regenerated by the caller.
    pub regenerate: Vec<String>,
}

impl TreeMerge {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty() && self.regenerate.is_empty()
    }
}

/// Merges two trees, given as path → content maps with `/`-separated paths,
/// against their merge base. `policy_for` gives each path's merge policy.
pub fn merge_trees<V: AsRef<[u8]>>(
    base: &BTreeMap<String, V>,
    ours: &BTreeMap<String, V>,
    theirs: &BTreeMap<String, V>,
    policy_for: impl Fn(&str) -> MergePolicy,
) -> TreeMerge {
    let paths: BTreeSet<&String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    let mut merge = TreeMerge::default();
    for path in paths {
        let [base_file, ours_file, theirs_file] =
            [base, ours, theirs].map(|tree| tree.get(path).map(AsRef::as_ref));
        match merge_file(base_file, ours_file, theirs_file, policy_for(path)) {
            FileMerge::Resolved(Some(content)) => {
                merge.files.insert(path.clone(), content);
            }
            FileMerge::Resolved(None) => {}
            FileMerge::Conflicted(conflict) => {
                merge.conflicts.insert(path.clone(), conflict);
            }
            FileMerge::Regenerate => merge.regenerate.push(path.clone()),
        }
    }
    move_file_directory_collisions(&mut merge);
    merge
}

/// A path kept as a file cannot also be the directory of another kept path.
/// Each side's tree was valid, so this only happens when one side added the
/// file and the other the directory; neither is picked silently.
fn move_file_directory_collisions(merge: &mut TreeMerge) {
    let colliding: Vec<String> = merge
        .files
        .keys()
        .filter(|path| {
            let directory = format!("{path}/");
            let mut later = merge.files.range::<String, _>(directory.clone()..);
            later
                .next()
                .is_some_and(|(next, _)| next.starts_with(&directory))
        })
        .cloned()
        .collect();
    for path in colliding {
        if let Some(content) = merge.files.remove(&path) {
            merge
                .conflicts
                .insert(path, FileConflict::FileDirectory { content });
        }
    }
}
