//! A change's edits, read from the object store: which files its head
//! changed against its parent, and whether two changes' edits of one file
//! overlap under the 3-line rule (SPEC §2 "Overlap").

use super::OverlapError;
use crate::history::trees::{self, Entry};
use rosen_core::{Change, ChangeId, ChangeParent, FileMode, ObjectId};
use rosen_merge::FileEdit;
use rosen_store::ObjectStore;
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

type ChangedFiles = BTreeMap<String, ChangedFile>;
type CacheKey = (ObjectId, ObjectId);
const CACHED_LINE_BYTES: usize = 8192;
const CACHE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
struct CachedFiles {
    entries: HashMap<CacheKey, ChangedFiles>,
    bytes: usize,
}

/// Event audiences repeatedly compare the same live heads. Keep only immutable
/// tree diffs here; claims, states and overlap policy are read afresh per query.
#[derive(Default)]
pub(crate) struct Cache(Mutex<CachedFiles>);

impl Cache {
    pub(crate) fn files(
        &self,
        store: &dyn ObjectStore,
        heads: &HashMap<ChangeId, ObjectId>,
        change: &Change,
    ) -> Result<ChangedFiles, OverlapError> {
        let parent = match change.parent {
            ChangeParent::Base(commit) => commit,
            ChangeParent::Change(below) => *heads
                .get(&below)
                .ok_or(OverlapError::MissingParent(change.id, below))?,
        };
        let key = (parent, change.head);
        if let Some(files) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .get(&key)
        {
            return Ok(files.clone());
        }
        // Never hold the cache lock across object reads. A moved parent or
        // child selects another key, including rebases that retain the head.
        let files = changed_files(store, heads, change)?;
        self.remember(key, &files);
        Ok(files)
    }

    fn remember(&self, key: CacheKey, files: &ChangedFiles) {
        // Reserve the lazy line result as well as each path and map entry.
        let bytes = files
            .keys()
            .map(|path| path.len() + std::mem::size_of::<ChangedFile>() + CACHED_LINE_BYTES + 64)
            .sum::<usize>();
        if bytes > CACHE_BYTES {
            return;
        }
        let mut cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.entries.contains_key(&key) {
            return;
        }
        if cache.entries.len() >= 2048 || cache.bytes + bytes > CACHE_BYTES {
            cache.entries.clear();
            cache.bytes = 0;
        }
        cache.entries.insert(key, files.clone());
        cache.bytes += bytes;
    }
}

/// One file a change edited, with its line regions: what [`read`] reports.
#[derive(Debug, Clone)]
pub struct EditedFile {
    pub parent: Option<Entry>,
    pub head: Option<Entry>,
    /// Zero-based, half-open spans in the parent's lines; insertions are empty
    /// spans. Non-text edits cover the whole file.
    pub regions: FileEdit,
}

/// One file a change edited, as an overlap query sees it. Most changed paths
/// are touched by no other change, so the line diff, and the parent bytes it
/// keeps for carrying, are computed only when [`edits_overlap`] first needs
/// them, then reused for every later comparison of the same file.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub parent: Option<Entry>,
    pub head: Option<Entry>,
    // Query copies share this immutable result, so cached tree diffs also
    // avoid rereading blobs and repeating line diffs across event audiences.
    diff: Arc<OnceLock<LineDiff>>,
    // Large blobs remain query-local instead of accumulating in the cache.
    local_diff: OnceCell<LineDiff>,
}

#[derive(Debug, Clone)]
struct LineDiff {
    regions: FileEdit,
    parent_bytes: Option<Vec<u8>>,
}

impl LineDiff {
    fn retained_bytes(&self) -> usize {
        let spans = match &self.regions {
            FileEdit::Whole => 0,
            FileEdit::Lines(spans) => {
                spans.capacity() * std::mem::size_of::<rosen_merge::LineSpan>()
            }
        };
        spans + self.parent_bytes.as_ref().map_or(0, Vec::capacity)
    }
}

impl ChangedFile {
    fn diff(&self, store: &dyn ObjectStore) -> Result<&LineDiff, OverlapError> {
        if let Some(diff) = self.diff.get() {
            return Ok(diff);
        }
        if let Some(diff) = self.local_diff.get() {
            return Ok(diff);
        }
        let diff = file_edit(store, self.parent, self.head)?;
        if diff.retained_bytes() > CACHED_LINE_BYTES {
            return Ok(self.local_diff.get_or_init(|| diff));
        }
        Ok(self.diff.get_or_init(|| diff))
    }
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
    let files = changed_files(&store, &heads, &current)?
        .into_iter()
        .map(|(path, file)| {
            let regions = file_edit(&store, file.parent, file.head)?.regions;
            let (parent, head) = (file.parent, file.head);
            Ok((
                path,
                EditedFile {
                    parent,
                    head,
                    regions,
                },
            ))
        })
        .collect::<Result<_, OverlapError>>()?;
    Ok(LiveEdits {
        change: current.id,
        head: current.head,
        parent_tree: parent_tree(&store, &heads, &current)?,
        files,
    })
}

/// Every file `change` edited against its parent, by path. The parent is the
/// base commit's tree, or the head of the change it is stacked on. This reads
/// only trees; no file is diffed until an overlap test asks for its regions.
pub fn changed_files(
    store: &dyn ObjectStore,
    heads: &HashMap<ChangeId, ObjectId>,
    change: &Change,
) -> Result<BTreeMap<String, ChangedFile>, OverlapError> {
    let parent_tree = parent_tree(store, heads, change)?;
    let changed = trees::changed_files(store, Some(parent_tree), Some(change.head), &[])?;
    Ok(changed
        .into_iter()
        .map(|file| {
            let edit = ChangedFile {
                parent: file.old,
                head: file.new,
                diff: Arc::default(),
                local_diff: OnceCell::new(),
            };
            (file.path, edit)
        })
        .collect())
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
pub fn edits_overlap(
    store: &dyn ObjectStore,
    ours: &ChangedFile,
    theirs: &ChangedFile,
) -> Result<bool, OverlapError> {
    let ours = ours.diff(store)?;
    let theirs = theirs.diff(store)?;
    let (FileEdit::Lines(_), FileEdit::Lines(_)) = (&ours.regions, &theirs.regions) else {
        return Ok(true);
    };
    // A line edit always has a parent file: `file_edit` makes an added or
    // deleted file a whole-file edit.
    let (Some(our_parent), Some(their_parent)) = (&ours.parent_bytes, &theirs.parent_bytes) else {
        return Ok(true);
    };
    let ours_carried = ours.regions.carried(our_parent, their_parent);
    Ok(ours_carried.overlaps(&theirs.regions))
}

/// The edit one file underwent, with its parent's bytes for carrying.
fn file_edit(
    store: &dyn ObjectStore,
    parent: Option<Entry>,
    head: Option<Entry>,
) -> Result<LineDiff, OverlapError> {
    let whole = LineDiff {
        regions: FileEdit::Whole,
        parent_bytes: None,
    };
    let (Some(parent), Some(head)) = (parent, head) else {
        return Ok(whole);
    };
    // A mode change, symlink or gitlink has no lines to tell apart.
    if parent.mode != head.mode || !is_text_mode(parent.mode) {
        return Ok(whole);
    }
    let parent_bytes = store.read_blob(&parent.id)?.data;
    let head_bytes = store.read_blob(&head.id)?.data;
    let regions = FileEdit::between(Some(&parent_bytes), Some(&head_bytes));
    Ok(LineDiff {
        regions,
        parent_bytes: Some(parent_bytes),
    })
}

fn is_text_mode(mode: FileMode) -> bool {
    matches!(mode, FileMode::Regular | FileMode::Executable)
}
