//! A change's merge result: its edits since the base commit its stack sits
//! on, merged onto the current base (R5), written to the store so a checker
//! can build and test it without a workspace (R11, R8 step 1).
//!
//! Only files both sides changed are read and merged line by line; every
//! other file keeps the side that changed it, so the cost follows the size
//! of the two diffs, not of the project.

use super::revision::{base_commit, stack_root, Revision};
use super::trees::{apply, changed_files, content, Change, Entry};
use super::{Ctx, HistoryError, HistoryResult};
use rosen_core::{FileMode, ObjectId, Tree};
use rosen_merge::{merge_file, FileMerge, MergePolicy};
use rosen_protocol::MergeResultAnswer;
use rosen_store::ObjectStore;
use std::collections::BTreeMap;

pub fn merge_result(ctx: &Ctx, change: &str) -> HistoryResult<MergeResultAnswer> {
    let Revision::Change(change) = Revision::resolve(ctx, Some(change))? else {
        return Err(HistoryError::Invalid(
            "a merge result is computed for a change; name a change ID".into(),
        ));
    };
    if change.state.is_closed() {
        return Err(HistoryError::Refused(format!(
            "change {} is {}; only open and ready changes have a merge result to check",
            change.id, change.state
        )));
    }
    let (_, sits_on) = stack_root(ctx, &change)?;
    let base = base_commit(ctx)?;
    let (tree, conflicts) = if sits_on == base {
        (Some(change.head), Vec::new())
    } else {
        let merge_base = ctx.index.commit(&ctx.store, &sits_on)?.tree;
        let onto = ctx.index.commit(&ctx.store, &base)?.tree;
        merge_trees(ctx, merge_base, onto, change.head)?
    };
    Ok(MergeResultAnswer {
        change: change.id,
        head: change.head,
        base,
        tree,
        conflicts,
    })
}

/// Merges `theirs`' edits since `base` onto `ours`. Returns the stored
/// merged tree, or the paths that conflict.
///
/// Every path merges as text: per-path policies for generated files and
/// registration lists come from project configuration, which the server
/// does not read yet.
pub(crate) fn merge_trees(
    ctx: &Ctx,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
) -> HistoryResult<(Option<ObjectId>, Vec<String>)> {
    let ours_changes: BTreeMap<String, Change> =
        changed_files(&ctx.store, Some(base), Some(ours), &[])?
            .into_iter()
            .map(|c| (c.path.clone(), c))
            .collect();
    let mut edits = BTreeMap::new();
    let mut conflicts = Vec::new();
    for change in changed_files(&ctx.store, Some(base), Some(theirs), &[])? {
        match ours_changes.get(&change.path) {
            None => {
                edits.insert(change.path, change.new);
            }
            Some(ours) if ours.new == change.new => {}
            Some(ours) => match merge_one(ctx, change.old, ours.new, change.new)? {
                Some(merged) => {
                    edits.insert(change.path, merged);
                }
                None => conflicts.push(change.path),
            },
        }
    }
    let tree = apply(&ctx.store, Some(ours), &edits, &mut conflicts)?;
    if !conflicts.is_empty() {
        conflicts.sort();
        conflicts.dedup();
        return Ok((None, conflicts));
    }
    let tree = match tree {
        Some(tree) => tree,
        None => ctx.store.put_tree(&Tree::default())?,
    };
    Ok((Some(tree), Vec::new()))
}

/// Merges one file both sides changed. `Ok(None)` is a conflict;
/// `Ok(Some(None))` deletes the file.
fn merge_one(
    ctx: &Ctx,
    base: Option<Entry>,
    ours: Option<Entry>,
    theirs: Option<Entry>,
) -> HistoryResult<Option<Option<Entry>>> {
    // Two different commits of a submodule have no lines to merge.
    if [base, ours, theirs].iter().flatten().any(|e| !e.is_blob()) {
        return Ok(None);
    }
    let [base_data, ours_data, theirs_data] = [base, ours, theirs].map(|e| content(&ctx.store, e));
    let (base_data, ours_data, theirs_data) = (base_data?, ours_data?, theirs_data?);
    let merged = merge_file(
        base_data.as_deref(),
        ours_data.as_deref(),
        theirs_data.as_deref(),
        MergePolicy::Text,
    );
    match merged {
        FileMerge::Resolved(None) => Ok(Some(None)),
        FileMerge::Resolved(Some(data)) => Ok(Some(Some(Entry {
            mode: merged_mode(base, ours, theirs),
            id: ctx.store.put_blob(&data)?,
        }))),
        FileMerge::Conflicted(_) | FileMerge::Regenerate => Ok(None),
    }
}

/// Three-way on the mode: theirs wins only where theirs changed it.
fn merged_mode(base: Option<Entry>, ours: Option<Entry>, theirs: Option<Entry>) -> FileMode {
    let [base, ours, theirs] = [base, ours, theirs].map(|e| e.map(|e| e.mode));
    let chosen = if theirs == base { ours } else { theirs };
    chosen.or(ours).or(theirs).unwrap_or(FileMode::Regular)
}
