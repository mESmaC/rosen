//! The Rosen client library (SPEC.md D7, §3).
//!
//! Velocity links this crate directly, and the `rosen` CLI is a thin layer
//! over it. It covers daemon discovery and start, workspace
//! materialisation, the file watcher and snapshots, the offline journal,
//! and every step of an agent's flow from claim to land (SPEC.md §5).
//!
//! # Stable API
//!
//! - [`Session`] is the entry point: one agent's connection to its machine's
//!   daemon, which it finds or starts. It creates, watches, snapshots and
//!   deletes workspaces, and claims, reviews, submits receipts, lands and
//!   applies rebases under the agent's identity.
//! - [`ChannelMembership`] is the agent joined to the project's AIRC channel
//!   (R13) under the member ID and nick Velocity gave it: piece claims,
//!   messages and ledgers.
//! - [`Client`] holds one typed method per protocol request, for everything a
//!   session does not wrap. Its request and answer types are in
//!   [`protocol`], whose wire format is versioned, and the model types they
//!   carry are re-exported here.
//! - [`Error`] is non-exhaustive, so a new failure is not a breaking change.
//! - The [`workspace`] and [`snapshot`] modules are the lower-level pieces
//!   sessions are built from. The daemon uses them directly; a host should
//!   not need to.

pub mod bisect;
pub mod changes;
pub mod channel;
pub mod claims;
mod conflicts;
pub mod connection;
pub mod daemon;
pub mod discovery;
mod edits;
pub mod enrollment;
pub mod events;
pub mod history;
pub mod identity;
mod import;
mod land;
pub mod machines;
mod pinned_tls;
pub mod post_land;
pub mod project_config;
pub mod receipts;
pub mod reviews;
pub mod server_workspace;
mod session;
pub mod snapshot;
pub mod workspace;

pub use connection::Client;
pub use daemon::connect_or_start;
pub use land::land_reconnecting;
pub use rosen_core::{AgentId, ChangeId, ChangeParent, ChangeState, ObjectId};
pub use rosen_protocol as protocol;
pub use rosen_protocol::Endpoint;
pub use session::{
    ChannelMembership, CreatedWorkspace, MergeCheckout, Session, SessionBuilder, WorkClaim,
    WorkspaceSpec, WorkspaceWatch,
};
pub use snapshot::{SnapshotReport, WatchEvent};
pub use workspace::{Report as MaterialiseReport, Sparse};

use rosen_protocol::ErrorBody;

/// Errors a client operation can return.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The operation exists in the CLI and API surface but has no
    /// implementation in this build yet.
    #[error("`{operation}` is not implemented yet")]
    NotImplemented { operation: String },
    /// An argument was rejected before anything was sent to the server.
    #[error("{0}")]
    InvalidArgument(String),
    /// No server listens on the endpoint.
    #[error("no rosen server is running on {0}")]
    NotRunning(Endpoint),
    /// The daemon could not be launched, or did not come up in time.
    #[error("could not start rosen-server: {0}")]
    Start(String),
    /// The server refused the handshake, usually a protocol version mismatch.
    #[error(
        "the server refused the connection: {reason} (it speaks protocol versions {supported:?})"
    )]
    Rejected { reason: String, supported: Vec<u32> },
    /// The server answered the request with an error.
    #[error("server error: {0}")]
    Server(ErrorBody),
    /// The connection closed before the answer arrived.
    #[error("the connection to the server closed")]
    Disconnected,
    /// The server sent something the protocol does not allow here.
    #[error("protocol error: {0}")]
    Unexpected(String),
    /// A local workspace could not be materialised.
    #[error(transparent)]
    Workspace(workspace::MaterialiseError),
    /// [`Session::create_workspace`] created the change but not its
    /// workspace. The change still exists, so the caller can retry with
    /// [`Session::open_workspace`] or abandon it.
    #[error("created change {change} but not its workspace: {source}")]
    WorkspaceNotCreated {
        change: ChangeId,
        source: Box<Error>,
    },
    /// The server refused an offline journal record for good, as when the
    /// change landed or was abandoned while the client was away. The record
    /// and every later snapshot stay in the workspace's journal, so no edit
    /// is lost and R4 keeps the workspace; replay resumes once the change
    /// is open again.
    #[error(
        "the server refused offline snapshot {record} for good ({refusal}); it and later \
         snapshots stay in the workspace's journal until the change is open again"
    )]
    JournalHalted { record: u64, refusal: ErrorBody },
    /// A workspace could not be scanned or snapshotted.
    #[error(transparent)]
    Snapshot(#[from] snapshot::SnapshotError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result type for client operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Runs a client operation by its command path, for example `change new`.
/// CLI scaffolding for commands whose feature has not landed, not part of
/// the stable API.
///
/// Every operation without a typed method yet reports
/// [`Error::NotImplemented`]; each feature replaces its operation with a typed
/// method as it lands.
#[doc(hidden)]
pub fn run_operation(operation: &str) -> Result<()> {
    Err(Error::NotImplemented {
        operation: operation.to_owned(),
    })
}
