//! Diff between two revisions as unified hunks, from the same line matching
//! the merge uses, so a reviewer sees the edits the merge will see.

use super::revision::{parent_tree, Revision};
use super::trees::{changed_files, content, normalise, Change};
use super::{Ctx, HistoryError, HistoryResult};
use rosen_core::{ObjectId, Tree};
use rosen_protocol::{
    Diff, DiffHunk, DiffLine, DiffRequest, FileDiff, FileStatus, FileVersion, LineKind,
};
use rosen_store::ObjectStore;

const DEFAULT_CONTEXT: u32 = 3;
/// Git's test for binary content: a NUL byte in the first 8000 bytes.
const BINARY_PROBE: usize = 8000;

pub fn diff(ctx: &Ctx, request: &DiffRequest) -> HistoryResult<Diff> {
    let filters = request
        .paths
        .iter()
        .map(|p| normalise(p))
        .collect::<HistoryResult<Vec<_>>>()?;
    let to_rev = Revision::resolve(ctx, request.to.as_deref())?;
    let to = to_rev.tree(ctx)?;
    let from = match &request.from {
        Some(text) => Revision::resolve(ctx, Some(text))?.tree(ctx)?,
        None => default_from(ctx, &to_rev)?,
    };
    let context = request.context.unwrap_or(DEFAULT_CONTEXT) as usize;
    let mut files = Vec::new();
    for change in changed_files(&ctx.store, Some(from), Some(to), &filters)? {
        files.push(file_diff(ctx, change, context)?);
    }
    Ok(Diff { from, to, files })
}

/// A change diffs against its parent, a commit against its first parent,
/// and a root commit against the empty tree.
fn default_from(ctx: &Ctx, to: &Revision) -> HistoryResult<ObjectId> {
    match to {
        Revision::Change(change) => parent_tree(ctx, change),
        Revision::Commit(commit) => match ctx.index.commit(&ctx.store, commit)?.first_parent() {
            Some(parent) => Ok(ctx.index.commit(&ctx.store, &parent)?.tree),
            None => Ok(ctx.store.put_tree(&Tree::default())?),
        },
        Revision::Tree(tree) => Err(HistoryError::Invalid(format!(
            "{tree} is a tree with no parent; give the old side with `from`"
        ))),
    }
}

fn file_diff(ctx: &Ctx, change: Change, context: usize) -> HistoryResult<FileDiff> {
    let status = match (&change.old, &change.new) {
        (None, _) => FileStatus::Added,
        (_, None) => FileStatus::Deleted,
        _ => FileStatus::Modified,
    };
    let version = |e: &Option<super::trees::Entry>| {
        e.map(|e| FileVersion {
            mode: e.mode.as_octal().into_owned(),
            id: e.id,
        })
    };
    let (old, new) = (version(&change.old), version(&change.new));
    let old_data = content(&ctx.store, change.old)?.unwrap_or_default();
    let new_data = content(&ctx.store, change.new)?.unwrap_or_default();
    let binary = is_binary(&old_data) || is_binary(&new_data);
    let hunks = if binary {
        Vec::new()
    } else {
        hunks(&old_data, &new_data, context)
    };
    Ok(FileDiff {
        path: change.path,
        status,
        old,
        new,
        binary,
        hunks,
    })
}

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_PROBE)].contains(&0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep(usize, usize),
    Remove(usize),
    Add(usize),
}

/// The edit script: within each gap between kept lines, removals come
/// before additions, as in `diff -u`.
fn edit_script(old_len: usize, new_len: usize, matches: &[Option<usize>]) -> Vec<Op> {
    let mut ops = Vec::with_capacity(old_len.max(new_len));
    let mut next_new = 0;
    for (old_line, kept_as) in matches.iter().enumerate() {
        match kept_as {
            Some(new_line) => {
                ops.extend((next_new..*new_line).map(Op::Add));
                ops.push(Op::Keep(old_line, *new_line));
                next_new = new_line + 1;
            }
            None => ops.push(Op::Remove(old_line)),
        }
    }
    ops.extend((next_new..new_len).map(Op::Add));
    ops
}

fn hunks(old: &[u8], new: &[u8], context: usize) -> Vec<DiffHunk> {
    let old_lines: Vec<&[u8]> = old.split_inclusive(|&b| b == b'\n').collect();
    let new_lines: Vec<&[u8]> = new.split_inclusive(|&b| b == b'\n').collect();
    let matches = rosen_merge::match_file_lines(old, new);
    let ops = edit_script(old_lines.len(), new_lines.len(), &matches);
    let changed: Vec<usize> = (0..ops.len())
        .filter(|&i| !matches!(ops[i], Op::Keep(..)))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < changed.len() {
        // Extend the hunk while the next change is close enough that the
        // context of the two would touch.
        let mut j = i;
        while j + 1 < changed.len() && changed[j + 1] - changed[j] <= 2 * context + 1 {
            j += 1;
        }
        let first = changed[i].saturating_sub(context);
        let last = (changed[j] + context).min(ops.len() - 1);
        out.push(hunk(
            &ops[first..=last],
            &ops[..first],
            &old_lines,
            &new_lines,
        ));
        i = j + 1;
    }
    out
}

fn hunk(ops: &[Op], before: &[Op], old: &[&[u8]], new: &[&[u8]]) -> DiffHunk {
    let old_before = before.iter().filter(|o| !matches!(o, Op::Add(_))).count() as u32;
    let new_before = before
        .iter()
        .filter(|o| !matches!(o, Op::Remove(_)))
        .count() as u32;
    let mut lines = Vec::with_capacity(ops.len());
    let (mut old_count, mut new_count) = (0u32, 0u32);
    for op in ops {
        let (kind, text) = match *op {
            Op::Keep(o, _) => (LineKind::Context, old[o]),
            Op::Remove(o) => (LineKind::Removed, old[o]),
            Op::Add(n) => (LineKind::Added, new[n]),
        };
        old_count += u32::from(kind != LineKind::Added);
        new_count += u32::from(kind != LineKind::Removed);
        lines.push(DiffLine {
            kind,
            text: text_of(text),
        });
    }
    // An empty side names the line before the hunk, as `diff -u` does.
    let start = |before: u32, count: u32| if count == 0 { before } else { before + 1 };
    DiffHunk {
        old_start: start(old_before, old_count),
        old_lines: old_count,
        new_start: start(new_before, new_count),
        new_lines: new_count,
        lines,
    }
}

fn text_of(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8_lossy(line).into_owned()
}
