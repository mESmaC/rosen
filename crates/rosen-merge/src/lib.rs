//! Rosen merging: three-way file merge, conflict objects and overlap
//! detection (SPEC.md R5, R6).
//!
//! Only the server links this crate. Clients never run merge logic (§3).
//!
//! - [`merge_trees`] merges every path of two trees against their merge base.
//! - [`merge_file`] merges one path: additions, deletions, binary files and
//!   per-path policies for generated files and registration lists.
//! - [`merge_text`] merges one text file line by line (diff3). Each region
//!   that does not merge cleanly becomes a [`ConflictRegion`] holding base,
//!   ours and theirs, never a silent pick.
//! - [`match_file_lines`] pairs the lines two versions of a file share.

mod diff;
mod file;
mod lines;
mod merge;

pub use file::{merge_file, merge_trees, FileConflict, FileMerge, MergePolicy, Side, TreeMerge};
pub use merge::{merge_text, ConflictRegion, Hunk, MarkerLabels, TextMerge};

/// Pairs each line of `old` with the line of `new` it is kept as, or `None`
/// where it was deleted, using the same diff the merge does. History queries
/// build blame and unified diffs from it (SPEC R11). Lines keep their
/// terminators, as in the merge.
pub fn match_file_lines(old: &[u8], new: &[u8]) -> Vec<Option<usize>> {
    let (old, new) = (lines::split_lines(old), lines::split_lines(new));
    let mut interner = lines::LineInterner::default();
    let old_ids = interner.intern_all(&old);
    let new_ids = interner.intern_all(&new);
    diff::match_lines(&old_ids, &new_ids)
}
