//! The Rosen client–server protocol (SPEC §3 "Protocol"): versioned, framed
//! request/response plus one event stream per client, over a transport that does
//! not assume a local peer.
//!
//! Both `rosen-server` and `rosen-client` depend on this crate, so the client never
//! links server code and the server never links client code.

pub mod change;
pub mod claim;
pub mod frame;
pub mod message;
pub mod snapshot;
pub mod transport;

pub use change::{ChangeFilter, ChangeRecord, NewChange};
pub use claim::{
    ClaimFilter, ClaimHolder, ClaimKind, ClaimOutcome, ClaimRequest, PathClaim, ReleaseOutcome,
    ShareOutcome, WaitOutcome, WokenWait,
};
pub use message::*;
pub use snapshot::{
    ObjectUpload, ObjectsStored, SnapshotOutcome, SnapshotRequest, UploadKind, UploadObject,
};
pub use transport::{connect, BoxStream, Endpoint, Listener};
