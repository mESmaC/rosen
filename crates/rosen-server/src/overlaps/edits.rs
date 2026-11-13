//! A change's edits, read from the object store: which files its head
//! changed against its parent, and whether two changes' edits of one file
//! overlap under the 3-line rule (SPEC §2 "Overlap").

use super::OverlapError;
use crate::history::trees::{self, Entry};
use rosen_core::{Change, ChangeId, ChangeParent, FileMode, ObjectId};
use rosen_merge::FileEdit;
use rosen_store::ObjectStore;
use std::collections::{BTreeMap, HashMap};

/// One file a change edited: its entry in the parent and in the head.
#[derive(Debug, Clone)]
pub struct EditedFile {
    pub parent: Option<Entry>,
    pub head: Option<Entry>,
}

/// Every file `change` edited against its parent, by path. The parent is the
/// base commit's tree, or the head of the change it is stacked on.
pub fn edited_files(
    store: &dyn ObjectStore,
    heads: &HashMap<ChangeId, ObjectId>,
    change: &Change,
) -> Result<BTreeMap<String, EditedFile>, OverlapError> {
    let parent_tree = match change.parent {
        ChangeParent::Base(commit) => store.read_commit(&commit)?.tree,
        ChangeParent::Change(below) => *heads
            .get(&below)
            .ok_or_else(|| OverlapError::MissingParent(change.id, below))?,
    };
    let changed = trees::changed_files(store, Some(parent_tree), Some(change.head), &[])?;
    Ok(changed
        .into_iter()
        .map(|file| {
            let edited = EditedFile {
                parent: file.old,
                head: file.new,
            };
            (file.path, edited)
        })
        .collect())
}

/// Whether two changes' edits of the same path overlap. Edits against
/// different parent versions are compared in the second one's lines.
pub fn edits_overlap(
    store: &dyn ObjectStore,
    ours: &EditedFile,
    theirs: &EditedFile,
) -> Result<bool, OverlapError> {
    let (our_edit, our_parent) = file_edit(store, ours)?;
    let (their_edit, their_parent) = file_edit(store, theirs)?;
    let (FileEdit::Lines(_), FileEdit::Lines(_)) = (&our_edit, &their_edit) else {
        return Ok(true);
    };
    // A line edit always has a parent file: `file_edit` makes an added or
    // deleted file a whole-file edit.
    let (Some(our_parent), Some(their_parent)) = (our_parent, their_parent) else {
        return Ok(true);
    };
    let ours_carried = our_edit.carried(&our_parent, &their_parent);
    Ok(ours_carried.overlaps(&their_edit))
}

/// The edit one file underwent, with its parent's bytes for carrying.
fn file_edit(
    store: &dyn ObjectStore,
    file: &EditedFile,
) -> Result<(FileEdit, Option<Vec<u8>>), OverlapError> {
    let (Some(parent), Some(head)) = (file.parent, file.head) else {
        return Ok((FileEdit::Whole, None));
    };
    // A mode change, symlink or gitlink has no lines to tell apart.
    if parent.mode != head.mode || !is_text_mode(parent.mode) {
        return Ok((FileEdit::Whole, None));
    }
    let parent_bytes = store.read_blob(&parent.id)?.data;
    let head_bytes = store.read_blob(&head.id)?.data;
    let edit = FileEdit::between(Some(&parent_bytes), Some(&head_bytes));
    Ok((edit, Some(parent_bytes)))
}

fn is_text_mode(mode: FileMode) -> bool {
    matches!(mode, FileMode::Regular | FileMode::Executable)
}
