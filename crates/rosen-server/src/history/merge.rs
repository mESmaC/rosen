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
    let merged = if sits_on == base {
        TreeMerge {
            tree: Some(change.head),
            ..TreeMerge::default()
        }
    } else {
        // The configuration in force on the base decides how each path
        // merges, as it will for the land onto that base.
        let config = ctx.state.project_config.at_commit(&base)?.config;
        let policy = |path: &str| {
            if config.is_generated(path) {
                MergePolicy::Generated
            } else if config.is_registration_list(path) {
                MergePolicy::RegistrationList
            } else {
                MergePolicy::Text
            }
        };
        let merge_base = ctx.index.commit(&ctx.store, &sits_on)?.tree;
        let onto = ctx.index.commit(&ctx.store, &base)?.tree;
        merge_trees(ctx, [merge_base, onto, change.head], &policy)?
    };
    Ok(MergeResultAnswer {
        change: change.id,
        head: change.head,
        base,
        tree: merged.tree,
        conflicts: merged.conflicts,
        regenerate: merged.regenerate,
    })
}

/// The result of merging two trees.
#[derive(Debug, Default)]
pub(crate) struct TreeMerge {
    /// The stored merged tree; `None` when any path conflicts.
    pub tree: Option<ObjectId>,
    pub conflicts: Vec<String>,
    /// Generated files both sides changed. The tree holds the base's
    /// version; whoever builds the result regenerates them (R5, D9).
    pub regenerate: Vec<String>,
}

/// What merging one file both sides changed gives.
enum FileOutcome {
    /// The merged file, or `None` when the merge deletes it.
    Merged(Option<Entry>),
    Conflict,
    Regenerate,
}

/// Merges `theirs`' edits since `base` onto `ours`, given as
/// `[base, ours, theirs]` trees, with each path merged under `policy`.
pub(crate) fn merge_trees(
    ctx: &Ctx,
    [base, ours, theirs]: [ObjectId; 3],
    policy: &dyn Fn(&str) -> MergePolicy,
) -> HistoryResult<TreeMerge> {
    let ours_changes: BTreeMap<String, Change> =
        changed_files(&ctx.store, Some(base), Some(ours), &[])?
            .into_iter()
            .map(|c| (c.path.clone(), c))
            .collect();
    let mut edits = BTreeMap::new();
    let mut merge = TreeMerge::default();
    for change in changed_files(&ctx.store, Some(base), Some(theirs), &[])? {
        let Some(ours) = ours_changes.get(&change.path) else {
            edits.insert(change.path, change.new);
            continue;
        };
        if ours.new == change.new {
            continue;
        }
        let sides = [change.old, ours.new, change.new];
        match merge_one(ctx, sides, policy(&change.path))? {
            FileOutcome::Merged(file) => {
                edits.insert(change.path, file);
            }
            FileOutcome::Conflict => merge.conflicts.push(change.path),
            FileOutcome::Regenerate => merge.regenerate.push(change.path),
        }
    }
    let tree = apply(&ctx.store, Some(ours), &edits, &mut merge.conflicts)?;
    if !merge.conflicts.is_empty() {
        merge.conflicts.sort();
        merge.conflicts.dedup();
        return Ok(merge);
    }
    merge.tree = Some(match tree {
        Some(tree) => tree,
        None => ctx.store.put_tree(&Tree::default())?,
    });
    Ok(merge)
}

/// Merges one file both sides changed, given as `[base, ours, theirs]`.
fn merge_one(
    ctx: &Ctx,
    sides: [Option<Entry>; 3],
    policy: MergePolicy,
) -> HistoryResult<FileOutcome> {
    // Two different commits of a submodule have no lines to merge.
    if sides.iter().flatten().any(|e| !e.is_blob()) {
        return Ok(FileOutcome::Conflict);
    }
    let [base, ours, theirs] = sides.map(|e| content(&ctx.store, e));
    let (base, ours, theirs) = (base?, ours?, theirs?);
    let merged = merge_file(base.as_deref(), ours.as_deref(), theirs.as_deref(), policy);
    Ok(match merged {
        FileMerge::Resolved(None) => FileOutcome::Merged(None),
        FileMerge::Resolved(Some(data)) => FileOutcome::Merged(Some(Entry {
            mode: merged_mode(sides),
            id: ctx.store.put_blob(&data)?,
        })),
        FileMerge::Conflicted(_) => FileOutcome::Conflict,
        FileMerge::Regenerate => FileOutcome::Regenerate,
    })
}

/// Three-way on the mode: theirs wins only where theirs changed it.
fn merged_mode(sides: [Option<Entry>; 3]) -> FileMode {
    let [base, ours, theirs] = sides.map(|e| e.map(|e| e.mode));
    let chosen = if theirs == base { ours } else { theirs };
    chosen.or(ours).or(theirs).unwrap_or(FileMode::Regular)
}
