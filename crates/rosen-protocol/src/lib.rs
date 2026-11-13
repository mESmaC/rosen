//! The Rosen client–server protocol (SPEC §3 "Protocol"): versioned, framed
//! request/response plus one event stream per client, over a transport that does
//! not assume a local peer.
//!
//! Both `rosen-server` and `rosen-client` depend on this crate, so the client never
//! links server code and the server never links client code.

pub mod change;
pub mod claim;
pub mod config;
pub mod frame;
pub mod history;
pub mod land;
pub mod message;
pub mod objects;
pub mod receipt;
pub mod review;
pub mod snapshot;
pub mod transport;
pub mod workspace;
pub use workspace::{WorkspaceBytes, WorkspaceWrite};

pub use change::{ChangeFilter, ChangeRecord, NewChange};
pub use claim::{
    ClaimFilter, ClaimHolder, ClaimKind, ClaimOutcome, ClaimRequest, PathClaim, ReleaseOutcome,
    ShareOutcome, WaitOutcome, WokenWait,
};
pub use config::{ConfigQuery, ConfigRecord};
pub use history::{
    Blame, BlameRange, BlameRequest, Diff, DiffHunk, DiffLine, DiffRequest, DirEntry, FileDiff,
    FileStatus, FileVersion, LineKind, Log, LogEntry, LogRequest, MergeBaseAnswer,
    MergeResultAnswer, ReadAnswer, ReadRequest,
};
pub use land::{LandAnswer, LandStatus};
pub use message::*;
pub use objects::{EnsureObjects, ObjectBatch, ObjectQuery, ObjectsLocal, WireKind, WireObject};
pub use receipt::{ReceiptRecord, SubmitReceipt};
pub use review::{
    CommentRequest, CommentTarget, ResolveRequest, ReviewComment, ReviewThread, ThreadFilter,
};
pub use snapshot::{
    ObjectUpload, ObjectsStored, SnapshotOutcome, SnapshotRequest, UploadKind, UploadObject,
};
pub use transport::{connect, BoxStream, Endpoint, Listener};
