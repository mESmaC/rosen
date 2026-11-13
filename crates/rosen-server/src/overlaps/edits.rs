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
    /// Zero-based, half-open spans in the parent's lines; insertions are empty
    /// spans. Non-text edits cover the whole file.
    pub regions: FileEdit,
    parent_bytes: Option<Vec<u8>>,
}

/// A consistent view of a change's durable live edits. Objects are immutable,
/// so these regions remain tied to `head` even if another snapshot arrives.
#[derive(Debug, Clone)]
pub struct LiveEdits {
    pub change: ChangeId,
    pub head: ObjectId,
    pub parent_tree: ObjectId,
    pub files: BTreeMap<String, EditedFile>,
}

/// Reads the latest snapshotted edits, including server workspace writes.
/// Reverting a path to its parent removes it from this view. Ignored local
/// paths never reach the snapshot tree and therefore have no regions.
pub fn read(state: &crate::ServerState, change: &str) -> Result<LiveEdits, OverlapError> {
    let (current, heads) = state.changes.with_conn(|conn| {
        let tx = conn.transaction()?;
        let current = crate::changes::get(&tx, crate::changes::resolve(&tx, change)?)?.change;
        let mut heads = HashMap::new();
        if let ChangeParent::Change(parent) = current.parent {
            heads.insert(parent, crate::changes::get(&tx, parent)?.change.head);
        }
        tx.commit()?;
        Ok((current, heads))
    })?;
    let store = rosen_store::LooseObjectStore::open(state.config.objects_dir())?;
    Ok(LiveEdits {
        change: current.id,
        head: current.head,
        parent_tree: parent_tree(&store, &heads, &current)?,
        files: edited_files(&store, &heads, &current)?,
    })
}

/// Every file `change` edited against its parent, by path. The parent is the
/// base commit's tree, or the head of the change it is stacked on.
pub fn edited_files(
    store: &dyn ObjectStore,
    heads: &HashMap<ChangeId, ObjectId>,
    change: &Change,
) -> Result<BTreeMap<String, EditedFile>, OverlapError> {
    let parent_tree = parent_tree(store, heads, change)?;
    let changed = trees::changed_files(store, Some(parent_tree), Some(change.head), &[])?;
    changed
        .into_iter()
        .map(|file| {
            let (regions, parent_bytes) = file_edit(store, file.old, file.new)?;
            Ok((
                file.path,
                EditedFile {
                    parent: file.old,
                    head: file.new,
                    regions,
                    parent_bytes,
                },
            ))
        })
        .collect()
}

fn parent_tree(
    store: &dyn ObjectStore,
    heads: &HashMap<ChangeId, ObjectId>,
    change: &Change,
) -> Result<ObjectId, OverlapError> {
    Ok(match change.parent {
        ChangeParent::Base(commit) => store.read_commit(&commit)?.tree,
        ChangeParent::Change(below) => *heads
            .get(&below)
            .ok_or_else(|| OverlapError::MissingParent(change.id, below))?,
    })
}

/// Runs object reads and region diffs off the async connection workers.
pub async fn handle(
    state: &std::sync::Arc<crate::ServerState>,
    change: String,
) -> rosen_protocol::Outcome {
    use rosen_protocol::{ErrorBody, ErrorCode, Outcome, Response};
    let state = std::sync::Arc::clone(state);
    match tokio::task::spawn_blocking(move || read(&state, &change)).await {
        Ok(Ok(view)) => Outcome::Ok(Response::EditRegions(view.into())),
        Ok(Err(err)) => {
            let code = match &err {
                OverlapError::Change(err) => crate::rpc::change_error_code(err),
                _ => ErrorCode::Internal,
            };
            Outcome::Error(ErrorBody::new(code, err.to_string()))
        }
        Err(err) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, err.to_string())),
    }
}

impl From<LiveEdits> for rosen_protocol::EditRegions {
    fn from(view: LiveEdits) -> Self {
        use rosen_protocol::{EditSpan, PathEdit, Regions};
        Self {
            change: view.change,
            head: view.head,
            parent_tree: view.parent_tree,
            files: view
                .files
                .into_iter()
                .map(|(path, edit)| PathEdit {
                    path,
                    regions: match edit.regions {
                        FileEdit::Whole => Regions::Whole,
                        FileEdit::Lines(spans) => Regions::Lines(
                            spans
                                .into_iter()
                                .map(|span| EditSpan {
                                    start: span.start,
                                    end: span.end,
                                })
                                .collect(),
                        ),
                    },
                })
                .collect(),
        }
    }
}

/// Whether two changes' edits of the same path overlap. Edits against
/// different parent versions are compared in the second one's lines.
pub fn edits_overlap(ours: &EditedFile, theirs: &EditedFile) -> bool {
    let (our_edit, our_parent) = (&ours.regions, &ours.parent_bytes);
    let (their_edit, their_parent) = (&theirs.regions, &theirs.parent_bytes);
    let (FileEdit::Lines(_), FileEdit::Lines(_)) = (&our_edit, &their_edit) else {
        return true;
    };
    // A line edit always has a parent file: `file_edit` makes an added or
    // deleted file a whole-file edit.
    let (Some(our_parent), Some(their_parent)) = (our_parent, their_parent) else {
        return true;
    };
    let ours_carried = our_edit.carried(our_parent, their_parent);
    ours_carried.overlaps(their_edit)
}

/// The edit one file underwent, with its parent's bytes for carrying.
fn file_edit(
    store: &dyn ObjectStore,
    parent: Option<Entry>,
    head: Option<Entry>,
) -> Result<(FileEdit, Option<Vec<u8>>), OverlapError> {
    let (Some(parent), Some(head)) = (parent, head) else {
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
