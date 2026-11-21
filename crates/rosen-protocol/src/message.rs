//! Wire messages exchanged between a Rosen client and the server.
//!
//! Every connection starts with a handshake: the client sends [`ClientFrame::Hello`]
//! listing the protocol versions it speaks, and the server answers with
//! [`ServerFrame::Welcome`] naming the version both sides will use, or
//! [`ServerFrame::Reject`]. After that the client sends requests tagged with an id
//! it chooses, and the server answers each with a response carrying the same id.
//! Responses may arrive out of order, and event frames are interleaved with them.

use crate::change::{ChangeFilter, ChangeRecord, NewChange};
use crate::channel::{ChannelAnswer, ChannelRequest};
use crate::claim::{
    ClaimFilter, ClaimOutcome, ClaimRequest, OverlapAnswer, OverlapQuery, PathClaim,
    ReleaseOutcome, ShareOutcome, WaitOutcome,
};
use crate::config::{ConfigQuery, ConfigRecord};
use crate::history::{
    Blame, BlameRequest, Diff, DiffRequest, Log, LogRequest, MergeBaseAnswer, MergeResultAnswer,
    ReadAnswer, ReadRequest,
};
use crate::land::{LandAnswer, LandCheck};
use crate::objects::{
    EnsureObjects, ObjectBatch, ObjectChunk, ObjectChunkQuery, ObjectQuery, ObjectsLocal,
};
use crate::receipt::{ReceiptRecord, SubmitReceipt};
use crate::review::{
    Approval, ApproveRequest, CommentRequest, LandedFilter, LandedReview, MarkReviewed,
    ResolveRequest, ReviewThread, ThreadFilter,
};
use crate::snapshot::{
    BlobChunk, BlobChunkStored, ObjectUpload, ObjectsStored, SnapshotOutcome, SnapshotRequest,
};
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
    /// Operator-only: adopt the specified Git mirror tip.
    MirrorAdopt {
        commit: rosen_core::ObjectId,
    },
    /// Operator-only: fast-forward the base to the Git mirror tip when the
    /// mirror gained lands from another server.
    MirrorCatchUp,
    /// Operator-only: remove literal paths from unmirrored base history.
    MirrorDropPaths {
        paths: Vec<String>,
    },
    /// Operator-only: the action kinds the server fetches from its Collapse
    /// endpoint and runs (R14 "Collapse action relay"); empty turns it off.
    RelaySet {
        allow: Vec<crate::RelayKind>,
    },
    /// Reads the next persisted checker job for a failed batch (R12).
    BisectNext {
        session: i64,
    },
    /// Submits one exact bisection job; stale results are refused.
    BisectSubmit(crate::BisectResult),
    MachineList,
    MachineApprove {
        machine: String,
    },
    MachineRevoke {
        machine: String,
    },
    JoinKeyList,
    JoinKeyRevoke {
        key: String,
    },
    JoinKeyCreate(crate::CreateJoinKey),
    MachineEnroll {
        key: String,
        name: String,
        /// The joining daemon's receipt public key (hex), bound to the new
        /// machine in the enrollment transaction (R8 step 1, R14). Absent
        /// from older clients, whose key is bound on first use instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt_key: Option<String>,
    },
    MachineRotate {
        credential: String,
    },
    MachinePending {
        name: String,
        role: crate::MachineRole,
        /// As for [`Request::MachineEnroll`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt_key: Option<String>,
    },
    /// Offers the newest untested landed commit to an external checker (R12).
    PostLandNext,
    /// Build/lint jobs use an independent verification prefix (R12).
    PostLandBuildNext,
    /// Confirmed failures, optionally belonging to one change.
    PostLandFailures {
        change: Option<String>,
    },
    /// Starts bisection from the stored green boundary after a batch failure.
    PostLandFailed {
        job: crate::PostLandJob,
        output: String,
    },
    /// Marks the tested landed prefix green after the full check set passed.
    PostLandPassed(crate::post_land::PostLandJob),
    /// Last built and last green, open bisections and pending inverses (R12).
    /// Reads only: it never offers a job or moves a session.
    PostLandStatus,
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
    /// Records the parent tree an edited stack from before parent trees
    /// were recorded started from, named by commit or tree (R8 step 3).
    ChangeRecoverBasis {
        change: String,
        basis: rosen_core::ObjectId,
    },
    /// Notices paths or globs, or opts into exclusive claims (R6). A refusal
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
    /// Sets a landed change's reviewed state (R7). Answered with the
    /// change's [`LandedReview`].
    ReviewMark(MarkReviewed),
    /// Lists landed changes with their reviewed state, oldest land first.
    ReviewLanded(LandedFilter),
    /// Records that a check set passed on a change's merge result onto one
    /// base commit (R8 step 1). Refused unless it names the change's current
    /// head, the pre-land check set and a commit the base ref has held.
    ReceiptSubmit(SubmitReceipt),
    /// Lands a change onto the base (R8 step 2): one commit, moved onto the
    /// base by compare-and-swap, only with no conflicts, a receipt that
    /// counts on the current base and, in a `before-land` project, an
    /// approval for the current head. Marks an
    /// open change ready first: asking to land is what `ready` means.
    Land {
        /// A change ID or unique prefix of one.
        change: String,
    },
    /// Reports every gate a land of a change would apply on the current
    /// base, without landing (R8 "Land check"). Changes nothing: it never
    /// marks the change ready, publishes an event or stores a receipt.
    LandCheck {
        /// A change ID or unique prefix of one.
        change: String,
    },
    /// Marks every open conflict stored in some files of a change resolved
    /// (`rosen resolve`, R5), in one transaction. Refused for a landed or
    /// abandoned change, and fails without changing anything when a path
    /// holds no conflict stored on the change.
    ConflictResolve(crate::ResolveConflicts),
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
    /// Stores one slice of a blob too large for a `PutObjects` frame (R2).
    PutBlobChunk(BlobChunk),
    /// Records a snapshot of a workspace's tree on its change: automatic
    /// from the watcher, or named by `rosen commit` (SPEC R2, D17).
    Snapshot(SnapshotRequest),
    /// Reads stored objects (D8). A machine's cache fills its store with it.
    GetObjects(ObjectQuery),
    /// Reads one slice of an object `GetObjects` lists as too large (D8).
    GetObjectChunk(ObjectChunkQuery),
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
    /// Writes a stored blob to a server-side file: a write too large for a
    /// `WorkspaceWrite` frame, sent first with `PutBlobChunk` (R3).
    WorkspaceWriteBlob(crate::WorkspaceBlobWrite),
    /// Machine-local warm pool size; zero disables it.
    PoolSize {
        size: usize,
    },
    /// Atomically takes a ready workspace matching this tree.
    PoolTake {
        tree: rosen_core::ObjectId,
        dest: String,
    },
    /// Whether every land after one base commit, up to and including a
    /// later one, was prose-only (R7), so a workspace prepared at the first
    /// needs no new prepare at the second (R3).
    LandedProse(crate::LandedProseQuery),
    /// An AIRC channel operation (R13). Answered only by the server, never
    /// from a machine's local copy.
    Channel(ChannelRequest),
}

/// Successful results, one per [`Request`] variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub enum Response {
    MirrorAdopted(crate::MirrorAdoption),
    MirrorCaughtUp(crate::MirrorCatchUp),
    MirrorRewritten(crate::HistoryRewrite),
    /// The answer to `relay_set`: the setting now stored.
    Relay(crate::RelaySetting),
    Bisection(crate::BisectState),
    Machines(Vec<crate::machine::MachineRecord>),
    Machine(crate::machine::MachineRecord),
    JoinKeys(Vec<crate::machine::JoinKeyRecord>),
    JoinKey(crate::machine::JoinKeyRecord),
    JoinKeyCreated {
        key: String,
    },
    MachineCredential(crate::MachineCredential),
    PostLand(crate::post_land::PostLandWork),
    PostLandFailures(Vec<crate::PostLandFailure>),
    PostLandStatus(crate::PostLandStatus),
    Pong,
    PoolSized,
    /// The handed-over workspace, or None when the pool had none to give.
    PoolTaken(Option<crate::PooledWorkspace>),
    /// The pool cannot serve takes into this destination at all, and why:
    /// as when the ready workspaces are on another volume (SPEC R3).
    PoolMissed(String),
    /// The answer to `landed_prose`: false when either commit is not in the
    /// base's landed history.
    LandedProse(bool),
    /// Boxed: `ServerStatus` is far larger than every other answer (clippy::large_enum_variant).
    Status(Box<ServerStatus>),
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
    BlobChunkStored(BlobChunkStored),
    Snapshot(SnapshotOutcome),
    EventsAcked(EventCursor),
    /// The answer to `review_comment` and `review_resolve`.
    ReviewThread(ReviewThread),
    ReviewThreads(Vec<ReviewThread>),
    Approval(Approval),
    Approvals(Vec<Approval>),
    LandedReview(LandedReview),
    LandedReviews(Vec<LandedReview>),
    Receipt(ReceiptRecord),
    Land(LandAnswer),
    /// Boxed: a check carries every gate's verdict.
    LandCheck(Box<LandCheck>),
    ConflictsResolved(crate::ConflictsResolved),
    Objects(ObjectBatch),
    ObjectChunk(ObjectChunk),
    ObjectsLocal(ObjectsLocal),
    Log(Log),
    Blame(Blame),
    Diff(Diff),
    MergeBase(MergeBaseAnswer),
    File(ReadAnswer),
    /// The answer to `ReadFile` for a file too large for one frame.
    LargeFile(crate::LargeFile),
    MergeResult(MergeResultAnswer),
    /// Boxed: a configuration is far larger than every other answer.
    Config(Box<ConfigRecord>),
    Channel(ChannelAnswer),
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
    /// Whether this server pushes the project's mirror (SPEC R1): only the
    /// server marked as its owner does. Absent on a cache daemon.
    #[serde(default)]
    pub mirror_owner: Option<bool>,
    /// Whether HTTPS mirror reads are using proactive basic auth after a
    /// not-found fallback succeeded in this server's lifetime (SPEC R1, #432).
    /// Absent on a cache daemon.
    #[serde(default)]
    pub mirror_proactive_auth: Option<bool>,
    /// The Collapse action relay's setting (R14). Absent on a cache daemon.
    #[serde(default)]
    pub relay: Option<crate::RelaySetting>,
    /// Health of the Collapse state feed, ledger push and action relay (R9,
    /// R10, #434). Absent when the base names no `collapse` root or on a cache
    /// daemon.
    #[serde(default)]
    pub collapse: Option<crate::CollapseHealth>,
    /// The git commit the daemon was built from, lowercase hex. `server`
    /// carries the crate version, which every commit shares, so this is what
    /// tells one build from another. Absent when the build could not tell.
    #[serde(default)]
    pub commit: Option<String>,
    /// The path of the daemon's executable on its own machine.
    #[serde(default)]
    pub executable: Option<String>,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    /// Missing or damaged objects to resend before retrying a snapshot.
    /// Older peers may omit this hint and still report the same error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects_needed: Vec<rosen_core::ObjectId>,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            objects_needed: Vec::new(),
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
    /// A merge left stored conflicts; `detail.paths` names the files (R5),
    /// and `detail.onto`, when present, the base commit merged in.
    Conflict,
    /// The change's state changed other than by landing; `detail.state`.
    Updated,
    /// A review comment, approval or resolution on the change (R7).
    Review,
    /// The change claimed `detail.paths`; `detail.overlaps` names concurrent work (R6).
    Claim,
    /// Claims were released: the change's own (`detail.paths`), or a wake
    /// for a waiting change (`detail.path`, `detail.wake`).
    Released,
    /// Post-land checks passed up to a commit (R12).
    Green,
    /// Build and lint passed through this commit; full tests have their own prefix.
    Built,
    /// The change was reverted after failing post-land checks (R12).
    Reverted,
    /// An inverse was refused; detail names the culprit, base, overlaps and reason.
    RevertRefused,
    /// A confirmed post-land failure, including its check kind and culprit.
    PostLandFailure,
    /// A bisection opened, accepted a step result, confirmed its culprit or
    /// stopped as flaky (R12). Sent only to the project view.
    Bisect,
    /// A git mirror push failed or recovered (R1). Sent to every subscriber.
    Mirror,
    /// An AIRC channel line (R13), with its channel-specific payload in detail.
    /// Uses the same sequence, subscription and acknowledgement as R9 events.
    Channel,
    /// A ledger push to the configured endpoint failed or recovered (R13).
    /// Sent to every subscriber.
    LedgerPush,
    /// The state feed to the project's Collapse endpoint failed, stopped or
    /// recovered (R9). Sent to every subscriber.
    StatePush,
    /// The Collapse action relay's setting changed, it ran or refused an
    /// action, or reaching Collapse failed or recovered (R14). Sent to
    /// every subscriber.
    Relay,
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
