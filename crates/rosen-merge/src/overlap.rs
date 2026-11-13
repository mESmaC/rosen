//! Edit regions and the 3-line overlap rule (SPEC §2 "Overlap", R6).
//!
//! A change's edits to one file are spans of its parent's lines. Two changes
//! overlap in a file when a span of one touches or sits within
//! [`OVERLAP_DISTANCE`] lines of a span of the other. Spans of changes with
//! different parents are first carried onto one parent's lines, because the
//! same line number means different text in two versions of a file.

use crate::diff::match_lines;
use crate::file::is_binary;
use crate::lines::{split_lines, LineInterner};

/// Edits this many unchanged lines apart, or closer, overlap (§2).
pub const OVERLAP_DISTANCE: usize = 3;

/// Lines `start..end` of the parent file were replaced. An empty span is a
/// pure insertion before line `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSpan {
    pub start: usize,
    pub end: usize,
}

impl LineSpan {
    /// Whether the spans touch or have at most [`OVERLAP_DISTANCE`] lines
    /// between them.
    pub fn near(self, other: LineSpan) -> bool {
        self.start <= other.end + OVERLAP_DISTANCE && other.start <= self.end + OVERLAP_DISTANCE
    }
}

/// How a change edited one path, against its parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEdit {
    /// The file was added, deleted, changed mode or is binary: every line
    /// counts as edited, since no line-level merge can separate two edits.
    Whole,
    /// The parent's lines each span replaced, in order and disjoint.
    Lines(Vec<LineSpan>),
}

impl FileEdit {
    /// The edit `parent -> head` makes. `None` is the file absent.
    pub fn between(parent: Option<&[u8]>, head: Option<&[u8]>) -> FileEdit {
        match (parent, head) {
            (Some(parent), Some(head)) if !is_binary(parent) && !is_binary(head) => {
                FileEdit::Lines(edit_spans(parent, head))
            }
            _ => FileEdit::Whole,
        }
    }

    /// Whether two edits of one file overlap. Both must be in the same
    /// parent's lines; see [`FileEdit::carried`].
    pub fn overlaps(&self, other: &FileEdit) -> bool {
        match (self, other) {
            (FileEdit::Whole, _) | (_, FileEdit::Whole) => true,
            (FileEdit::Lines(ours), FileEdit::Lines(theirs)) => spans_near(ours, theirs),
        }
    }

    /// This edit, made against `from`, in the lines of `onto`. A span widens
    /// to cover every line of `onto` with no counterpart in `from` next to
    /// it, so carrying never hides an overlap: it can only report a near one.
    pub fn carried(&self, from: &[u8], onto: &[u8]) -> FileEdit {
        match self {
            FileEdit::Whole => FileEdit::Whole,
            FileEdit::Lines(spans) if from == onto => FileEdit::Lines(spans.clone()),
            FileEdit::Lines(spans) => FileEdit::Lines(carry_spans(spans, from, onto)),
        }
    }
}

/// The parent spans `head` replaced, from the same diff the merge uses.
fn edit_spans(parent: &[u8], head: &[u8]) -> Vec<LineSpan> {
    let matches = line_matches(parent, head);
    let head_len = split_lines(head).len();
    let mut spans = Vec::new();
    let (mut start, mut next_head) = (0, 0);
    // Each kept line, and the end of both files, anchors the edit before it:
    // parent lines since the last anchor were deleted, head lines since it
    // were inserted.
    for at in 0..=matches.len() {
        let anchor = if at == matches.len() {
            Some(head_len)
        } else {
            matches[at]
        };
        let Some(head_at) = anchor else { continue };
        if start < at || next_head < head_at {
            spans.push(LineSpan { start, end: at });
        }
        start = at + 1;
        next_head = head_at + 1;
    }
    spans
}

/// Maps each span's ends through the lines `from` and `onto` share: the start
/// goes just past the last kept line before it, the end to the first kept
/// line at or after it.
fn carry_spans(spans: &[LineSpan], from: &[u8], onto: &[u8]) -> Vec<LineSpan> {
    let matches = line_matches(from, onto);
    let onto_len = split_lines(onto).len();
    // after_kept[p]: first `onto` line past every kept line before `from` line p.
    let mut after_kept = Vec::with_capacity(matches.len() + 1);
    after_kept.push(0);
    for kept in &matches {
        let last = *after_kept.last().unwrap_or(&0);
        after_kept.push(kept.map_or(last, |at| at + 1));
    }
    // next_kept[p]: the `onto` line of the first kept line at or after p.
    let mut next_kept = vec![onto_len; matches.len() + 1];
    for at in (0..matches.len()).rev() {
        next_kept[at] = matches[at].unwrap_or(next_kept[at + 1]);
    }
    spans
        .iter()
        .map(|span| {
            let start = after_kept[span.start.min(matches.len())];
            let end = next_kept[span.end.min(matches.len())].max(start);
            LineSpan { start, end }
        })
        .collect()
}

fn line_matches(old: &[u8], new: &[u8]) -> Vec<Option<usize>> {
    let (old, new) = (split_lines(old), split_lines(new));
    let mut interner = LineInterner::default();
    let old_ids = interner.intern_all(&old);
    let new_ids = interner.intern_all(&new);
    match_lines(&old_ids, &new_ids)
}

/// Both lists are sorted and disjoint, so one merge-style pass finds a near
/// pair if there is one.
fn spans_near(ours: &[LineSpan], theirs: &[LineSpan]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < ours.len() && j < theirs.len() {
        if ours[i].near(theirs[j]) {
            return true;
        }
        // The span that ends first cannot reach anything later in the other list.
        if ours[i].end < theirs[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    false
}
