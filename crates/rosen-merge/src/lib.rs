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

mod diff;
mod file;
mod lines;
mod merge;

pub use file::{merge_file, merge_trees, FileConflict, FileMerge, MergePolicy, Side, TreeMerge};
pub use merge::{merge_text, ConflictRegion, Hunk, MarkerLabels, TextMerge};
