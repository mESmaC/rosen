//! Rosen's automatic merge (SPEC R5): three-way merge at tree, file and line
//! level against the merge base. The server runs it for every rebase and
//! land; clients never do (SPEC §3).
//!
//! - [`merge_trees`] merges every path of two trees.
//! - [`merge_file`] merges one path: additions, deletions, binary files and
//!   per-path policies for generated files and registration lists.
//! - [`merge_text`] merges one text file line by line (diff3). Each region
//!   that does not merge cleanly becomes a [`ConflictRegion`] holding base,
//!   ours and theirs, never a silent pick.

mod diff;
mod file;
mod lines;
mod merge;

pub use file::{FileConflict, FileMerge, MergePolicy, Side, TreeMerge, merge_file, merge_trees};
pub use merge::{ConflictRegion, Hunk, MarkerLabels, TextMerge, merge_text};
