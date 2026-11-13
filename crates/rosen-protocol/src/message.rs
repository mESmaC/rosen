//! Wire messages exchanged between a Rosen client and the server.
//!
//! Every connection starts with a handshake: the client sends [`ClientFrame::Hello`]
//! listing the protocol versions it speaks, and the server answers with
//! [`ServerFrame::Welcome`] naming the version both sides will use, or
//! [`ServerFrame::Reject`]. After that the client sends requests tagged with an id
//! it chooses, and the server answers each with a response carrying the same id.
//! Responses may arrive out of order, and event frames are interleaved with them.

use crate::change::{ChangeFilter, ChangeRecord, NewChange};
use crate::claim::{
    ClaimFilter, ClaimOutcome, ClaimRequest, OverlapAnswer, OverlapQuery, PathClaim,
    ReleaseOutcome, ShareOutcome, WaitOutcome,
};
use crate::config::{ConfigQuery, ConfigRecord};
use crate::history::{
    Blame, BlameRequest, Diff, DiffRequest, Log, LogRequest, MergeBaseAnswer, MergeResultAnswer,
    ReadAnswer, ReadRequest,
};
use crate::land::LandAnswer;
use crate::objects::{EnsureObjects, ObjectBatch, ObjectQuery, ObjectsLocal};
use crate::receipt::{ReceiptRecord, SubmitReceipt};
use crate::review::{
    Approval, ApproveRequest, CommentRequest, ResolveRequest, ReviewThread, ThreadFilter,
};
use crate::snapshot::{ObjectUpload, ObjectsStored, SnapshotOutcome, SnapshotRequest};
use rosen_core::AgentId;
use serde::{Deserialize, Serialize};

/// The protocol version this build speaks. Bump it on any incompatible change to
/// the messages below; additive changes (new request variants) do not need a bump
/// because an unknown request is answered with [`ErrorCode::UnknownRequest`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Every protocol version this build can speak, newest first.
pub const SUPPORTED_VERSIONS: &[u32] = &[PROTOCOL_VERSION];

/// Picks the newest version both peers speak, if any.
pub fn negotiate_version(offered: &[u32]) -> Option<u32> {
    SUPPORTED_VERSIONS
        .iter()
        .copied()
        .find(|version| offered.contains(version))
}

/// A frame sent from client to server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// The first frame on every connection.
    Hello {
        /// Protocol versions the client speaks.
        versions: Vec<u32>,
        /// Free-form client name for diagnostics, for example `rosen-cli 0.1.0`.
        client: String,
        /// The machine credential a daemon on another machine presents to the
        /// server's network listener (R14). Local clients send none: the local
        /// transport is already limited to the OS user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<String>,
    },
    /// A request; the server answers with a [`ServerFrame::Response`] with the same id.
    Request { id: u64, request: Request },
    /// Starts this connection's event stream (SPEC R9).
    ///
    /// With a `subscriber`, the stream carries only the events that concern
    /// that agent, plus broadcasts, and `after: None` resumes after the
    /// subscriber's stored cursor (see [`Request::AckEvents`]), or from now if
    /// it has none. Without one, the stream is the project view: every event,
    /// from `after` or from now.
    Subscribe {
        after: Option<u64>,
        #[serde(default)]
        subscriber: Option<AgentId>,
    },
}

/// A frame sent from server to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Handshake accepted.
    Welcome {
        /// The version both sides now speak.
        version: u32,
        /// The server build, for diagnostics.
        server: String,
    },
    /// Handshake refused; the server closes the connection after sending it.
    Reject {
        reason: String,
        /// Versions the server would have accepted.
        supported: Vec<u32>,
    },
    /// The answer to the request with the same id.
    Response {
        id: u64,
        #[serde(flatten)]
        outcome: Outcome,
    },
    /// One event from the connection's event stream.
    Event(Event),
}

/// A request's result: exactly one of `ok` or `error` is present on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(Response),
    Error(ErrorBody),
}

/// Requests a client can make. Feature work adds variants here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// The server's identity, uptime and load.
    Status,
    /// Asks the daemon to stop accepting connections and exit.
    Shutdown,
    /// Creates a change with a new stable ID (R2).
    ChangeNew(NewChange),
    ChangeList(ChangeFilter),
    /// `change` is a full change ID or a unique prefix of one.
    ChangeShow {
        change: String,
    },
    /// Merge onto the current base without rewriting snapshots (R2).
    ChangeRebase {
        change: String,
    },
    ChangeAbandon {
        change: String,
    },
    /// Claims paths or globs for a change, first claim wins (R6). A refusal
    /// is an ordinary answer naming the holders, not an error.
    ClaimPaths(ClaimRequest),
    /// Records that a change edits paths another change may have claimed.
    SharePaths(ClaimRequest),
    /// Records a wait on claimed paths; a `released` event ends it.
    WaitPaths(ClaimRequest),
    /// Drops a change's claims, shares and waits on paths and wakes waiters.
    ReleasePaths(ClaimRequest),
    /// Lists claims, shares and waits on live changes.
    ClaimList(ClaimFilter),
    /// Reads the latest snapshotted paths and edit regions (R6).
    EditRegions {
        change: String,
    },
    /// Lists the live changes that overlap a change or a set of paths (R6).
    Overlaps(OverlapQuery),
    /// Stores the subscriber's event cursor: it has handled every event up
    /// to `seq`. A later subscribe with no cursor resumes after it, even
    /// across a server restart (R9). The cursor never moves backwards.
    AckEvents {
        subscriber: AgentId,
        seq: u64,
    },
    /// Starts a review thread on a line of a change's head, or replies in
    /// one (R7). Answered with the whole thread.
    ReviewComment(CommentRequest),
    ReviewApprove(ApproveRequest),
    ReviewApprovals {
        change: String,
    },
    /// Marks a review thread resolved. Resolving a resolved thread is a no-op.
    ReviewResolve(ResolveRequest),
    /// Lists a change's review threads with their comments, oldest first.
    ReviewList(ThreadFilter),
    /// Records that a check set passed on a change's merge result onto one
    /// base commit (R8 step 1). Refused unless it names the change's current
    /// head, the pre-land check set and a commit the base ref has held.
    ReceiptSubmit(SubmitReceipt),
    /// Lands a change onto the base (R8 step 2): one commit, moved onto the
    /// base by compare-and-swap, only with no conflicts, an approval for the
    /// current head and a receipt that counts on the current base. Marks an
    /// open change ready first: asking to land is what `ready` means.
    Land {
        /// A change ID or unique prefix of one.
        change: String,
    },
    /// Loads a git repository's history and makes one of its branches the
    /// project's base (SPEC R1). Refused once the project has a base.
    Import {
        /// An absolute local repository path, or a URL git can clone. The
        /// server resolves relative paths against its own directory, so
        /// clients send absolute ones.
        source: String,
        /// The branch to import; `None` takes the one the source's HEAD names.
        branch: Option<String>,
        /// The git remote the server pushes the base to after each land
        /// (SPEC R1): a URL, or an absolute path for the same reason as
        /// `source`. `None` leaves the project without a mirror.
        #[serde(default)]
        mirror: Option<String>,
    },
    /// Stores blobs and trees a snapshot needs (SPEC R2). Idempotent.
    PutObjects(ObjectUpload),
    /// Records a snapshot of a workspace's tree on its change: automatic
    /// from the watcher, or named by `rosen commit` (SPEC R2, D17).
    Snapshot(SnapshotRequest),
    /// Reads stored objects (D8). A machine's cache fills its store with it.
    GetObjects(ObjectQuery),
    /// Makes every object reachable from a root readable from this daemon's
    /// local store, fetching what a cache lacks from the server (D8, R3).
    /// Clients ask before reading a tree from the local store.
    EnsureObjects(EnsureObjects),
    /// The log of the base, a commit or a change, optionally limited to
    /// paths: path history (R11).
    Log(LogRequest),
    /// Which commit or change last changed each line of a file (R11).
    Blame(BlameRequest),
    /// A line diff between two revisions, or a revision and its parent (R11).
    Diff(DiffRequest),
    /// The merge base of two revisions (R11).
    MergeBase {
        a: String,
        b: String,
    },
    /// A file's bytes or a directory's listing at a revision (R11).
    ReadFile(ReadRequest),
    /// Merges a change onto the current base and stores the result, so a
    /// checker can read it without a workspace (R11, R8 step 1).
    MergeResult {
        change: String,
    },
    /// Reads the project configuration in force at a commit, by default the
    /// current base (SPEC "Project configuration").
    ConfigShow(ConfigQuery),
    /// Edits a server-side file and returns its stored snapshot (R3).
    WorkspaceWrite(crate::WorkspaceWrite),
    /// Machine-local warm pool size; zero disables it.
    PoolSize {
        size: usize,
    },
    /// Atomically takes a ready workspace matching this tree.
    PoolTake {
        tree: rosen_core::ObjectId,
        dest: String,
    },
}

/// Successful results, one per [`Request`] variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub enum Response {
    Pong,
    PoolSized,
    /// Number of tracked files, or None when no matching slot is ready.
    PoolTaken(Option<u64>),
    Status(ServerStatus),
    ShuttingDown,
    /// The answer to `change_new`, `change_show` and `change_abandon`.
    Change(ChangeRecord),
    Changes(Vec<ChangeRecord>),
    Imported(ImportSummary),
    Claimed(ClaimOutcome),
    Shared(ShareOutcome),
    Waiting(WaitOutcome),
    Released(ReleaseOutcome),
    PathClaims(Vec<PathClaim>),
    EditRegions(crate::edits::EditRegions),
    Overlaps(OverlapAnswer),
    ObjectsStored(ObjectsStored),
    Snapshot(SnapshotOutcome),
    EventsAcked(EventCursor),
    /// The answer to `review_comment` and `review_resolve`.
    ReviewThread(ReviewThread),
    ReviewThreads(Vec<ReviewThread>),
    Approval(Approval),
    Approvals(Vec<Approval>),
    Receipt(ReceiptRecord),
    Land(LandAnswer),
    Objects(ObjectBatch),
    ObjectsLocal(ObjectsLocal),
    Log(Log),
    Blame(Blame),
    Diff(Diff),
    MergeBase(MergeBaseAnswer),
    File(ReadAnswer),
    MergeResult(MergeResultAnswer),
    /// Boxed: a configuration is far larger than every other answer.
    Config(Box<ConfigRecord>),
}

/// The answer to [`Request::AckEvents`]: the cursor now stored, which is
/// higher than the acknowledged `seq` when an earlier ack went further.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventCursor {
    pub subscriber: AgentId,
    pub cursor: u64,
}

/// The answer to [`Request::Import`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportSummary {
    /// The base's name: the imported branch, for example `main`.
    pub branch: String,
    /// Hex id of the base commit: the branch's tip.
    pub base_commit: String,
    /// Objects reachable from the base commit, all now in the store.
    pub objects_reachable: u64,
    /// Objects this import wrote; the rest were already stored.
    pub objects_written: u64,
}

/// The answer to [`Request::Status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerStatus {
    /// The server build, for example `rosen-server 0.1.0`.
    pub server: String,
    pub protocol_version: u32,
    pub pid: u32,
    /// The endpoint the server listens on.
    pub endpoint: String,
    /// The directory holding the server's object store and metadata.
    pub data_dir: String,
    pub uptime_ms: u64,
    pub open_connections: u64,
    /// The sequence number of the newest event, 0 when there is none. A
    /// cache daemon reports 0: the event log is on the server.
    pub last_event: u64,
    /// The network listener's bound address, when the server has one (R14).
    #[serde(default)]
    pub listen: Option<String>,
    /// SHA-256 of the server's TLS certificate, lowercase hex. Daemons on
    /// other machines pin it. Absent on a cache daemon.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// The server a cache daemon forwards to; absent on the server itself.
    #[serde(default)]
    pub upstream: Option<String>,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ErrorBody {}

/// Machine-readable error categories. Clients branch on these, never on messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The frame could not be decoded, or arrived out of order.
    BadFrame,
    /// The request method is not known to this server build.
    UnknownRequest,
    /// The server is shutting down and takes no new work.
    ShuttingDown,
    /// The request was understood but failed inside the server.
    Internal,
    /// The request named something that does not exist, such as a change.
    NotFound,
    /// A parameter is malformed or ambiguous, such as a change ID prefix that
    /// matches more than one change.
    InvalidArgument,
    /// The request is valid but the current state forbids it, such as
    /// abandoning a landed change.
    Refused,
    /// A cache daemon could not reach the server, so it could not forward
    /// the request (D8). Retrying later may succeed.
    Unavailable,
}

/// The event types of SPEC R9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A snapshot of the change's workspace was stored (R2).
    Snapshot,
    /// The change landed; `detail.commit` is the landed commit (R8).
    Landed,
    /// The server rebased the change; `detail.onto` is its new parent commit.
    Rebased,
    /// A merge left stored conflicts; `detail.paths` names the files (R5).
    Conflict,
    /// The change's state changed other than by landing; `detail.state`.
    Updated,
    /// A review comment, approval or resolution on the change (R7).
    Review,
    /// The change claimed `detail.paths` (R6).
    Claim,
    /// Claims were released: the change's own (`detail.paths`), or a wake
    /// for a waiting change (`detail.path`, `detail.wake`).
    Released,
    /// Post-land checks passed up to a commit (R12).
    Green,
    /// The change was reverted after failing post-land checks (R12).
    Reverted,
    /// A git mirror push failed or recovered (R1). Sent to every subscriber.
    Mirror,
    /// An AIRC channel line (R13), with its channel-specific payload in detail.
    /// Uses the same sequence, subscription and acknowledgement as R9 events.
    Channel,
}

/// One event. `seq` is the cursor a client resumes from after a reconnect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub kind: EventKind,
    /// The change the event concerns, if any.
    pub change: Option<String>,
    /// Event-specific details; each event type documents its own shape.
    pub detail: serde_json::Value,
}

/// A connected client's frame that was not a valid [`ClientFrame`]. Kept separate
/// so the server can still answer a request whose method it does not know.
#[derive(Debug, Deserialize)]
pub struct RequestEnvelope {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub id: Option<u64>,
}
