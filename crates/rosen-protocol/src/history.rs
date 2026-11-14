//! Wire shapes for history queries (SPEC R11): log, path history, blame,
//! diff, merge base, file read and a change's merge result, answered by the
//! server with no checkout.
//!
//! Every query names a revision as text, resolved by the server:
//! - `base`, or no revision at all: the base ref's current commit;
//! - 40 hex digits: a commit or tree in the object store;
//! - anything else: a change ID or unique prefix of one (change IDs use only
//!   the letters k-z, so they never look like hex). A change reads as its
//!   current head tree, sitting on its parent.

use rosen_core::{AgentId, ChangeId, ObjectId, ReceiptTrailer};
use serde::{Deserialize, Serialize};

/// Parameters of [`crate::Request::Log`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRequest {
    /// Where the log starts. A change's log is its own snapshots, newest
    /// first, then the history of what it sits on.
    #[serde(default)]
    pub at: Option<String>,
    /// Path history: only commits that changed one of these files, or a file
    /// under one of these directories, against their first parent.
    #[serde(default)]
    pub paths: Vec<String>,
    /// At most this many entries; the server caps it.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// One commit in a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub commit: ObjectId,
    /// First parent first. Landed history is linear, so it has one.
    pub parents: Vec<ObjectId>,
    pub tree: ObjectId,
    /// `Name <email>`.
    pub author: String,
    /// Author time, Unix seconds.
    pub time: i64,
    /// The first line of the message.
    pub summary: String,
    /// The change the commit belongs to: a landed commit's `Rosen-Change`
    /// trailer, or the change whose snapshot it is.
    pub change: Option<ChangeId>,
    /// A landed commit's `Rosen-Receipt` trailer: the receipt its land
    /// counted. Absent for snapshots, imported history and lands that need
    /// no receipt (prose-only changes, server reverts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ReceiptTrailer>,
    /// A landed commit's `Rosen-Approved-By` trailers, in approval order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvers: Vec<AgentId>,
}

/// The answer to [`crate::Request::Log`], newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    pub entries: Vec<LogEntry>,
    /// More entries exist past the limit.
    pub truncated: bool,
}

/// Parameters of [`crate::Request::Blame`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameRequest {
    #[serde(default)]
    pub at: Option<String>,
    /// A repository-relative file path.
    pub path: String,
}

/// A run of consecutive lines that came from one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameRange {
    /// First line of the run in the blamed file, 1-based.
    pub start: u32,
    pub lines: u32,
    /// The commit that last changed these lines along first parents. `None`
    /// for lines the change being blamed has edited and not yet landed.
    pub commit: Option<ObjectId>,
    /// The line number of `start` in `commit`'s version of the file.
    pub original_start: u32,
    /// The change that made the edit, when it was a change.
    pub change: Option<ChangeId>,
}

/// The answer to [`crate::Request::Blame`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blame {
    pub path: String,
    /// The blob blamed.
    pub blob: ObjectId,
    pub ranges: Vec<BlameRange>,
    /// Author, time and summary of every commit `ranges` names.
    pub commits: Vec<LogEntry>,
}

/// Parameters of [`crate::Request::Diff`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRequest {
    /// The old side. `None` is the new side's parent: a change's parent, or a
    /// commit's first parent.
    #[serde(default)]
    pub from: Option<String>,
    /// The new side.
    #[serde(default)]
    pub to: Option<String>,
    /// Only these files, or files under these directories.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Unchanged lines shown around each hunk; defaults to 3.
    #[serde(default)]
    pub context: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
}

/// One side of a changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    /// Git's octal mode, for example `100644`.
    pub mode: String,
    pub id: ObjectId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: LineKind,
    /// The line without its terminator, decoded as UTF-8 with replacement.
    pub text: String,
}

/// A unified-diff hunk. Starts are 1-based; a side with no lines gives the
/// line before the hunk, as `diff -u` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub status: FileStatus,
    pub old: Option<FileVersion>,
    pub new: Option<FileVersion>,
    /// Either side is binary, so no hunks are given.
    pub binary: bool,
    pub hunks: Vec<DiffHunk>,
}

/// The answer to [`crate::Request::Diff`]: files in path order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    /// The two trees compared.
    pub from: ObjectId,
    pub to: ObjectId,
    pub files: Vec<FileDiff>,
}

/// The answer to [`crate::Request::MergeBase`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeBaseAnswer {
    /// The merge base three-way merge uses, or `None` for unrelated
    /// histories. A change counts from the base commit its stack sits on.
    pub merge_base: Option<ObjectId>,
    /// Every best common ancestor, sorted; more than one only after
    /// criss-cross merges in imported history.
    pub all: Vec<ObjectId>,
}

/// Parameters of [`crate::Request::ReadFile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    #[serde(default)]
    pub at: Option<String>,
    /// A file or directory path; empty for the root directory.
    #[serde(default)]
    pub path: String,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    /// Git's octal mode; `40000` is a directory.
    pub mode: String,
    pub id: ObjectId,
}

/// The answer to [`crate::Request::ReadFile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadAnswer {
    File {
        mode: String,
        id: ObjectId,
        /// Base64, because frames are JSON and files are arbitrary bytes.
        #[serde(with = "crate::snapshot::base64_bytes")]
        data: Vec<u8>,
    },
    Directory {
        id: ObjectId,
        entries: Vec<DirEntry>,
    },
}

/// The answer to [`crate::Request::ReadFile`] for a file too large for one
/// frame (#243). Its bytes are read in slices with
/// [`crate::Request::GetObjectChunk`]; the client's `read_file` does so and
/// answers [`ReadAnswer::File`]. A separate answer, not a field of `File`,
/// so a client that predates it fails to decode the answer instead of
/// reading the file as empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeFile {
    pub mode: String,
    pub id: ObjectId,
    /// The file's length in bytes.
    pub len: u64,
}

/// The answer to [`crate::Request::MergeResult`]: a change merged onto the
/// current base, as a checker builds it for a receipt (R8 step 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeResultAnswer {
    pub change: ChangeId,
    /// The change's head tree that was merged.
    pub head: ObjectId,
    /// The base commit merged onto: the receipt's `base`.
    pub base: ObjectId,
    /// The merged tree, stored on the server, so it can be read with
    /// `read_file` or materialised. `None` when the merge conflicts.
    pub tree: Option<ObjectId>,
    /// Paths that did not merge automatically.
    pub conflicts: Vec<String>,
    /// Generated files (project configuration) both sides changed. The tree
    /// holds the base's version; the checker regenerates them on the merge
    /// result before building, as Rosen runs no generators (R5, D9).
    #[serde(default)]
    pub regenerate: Vec<String>,
}
