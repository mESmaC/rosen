//! The Rosen client library (SPEC.md D7, §3).
//!
//! Velocity links this crate directly, and the `rosen` CLI is a thin layer
//! over it. It covers daemon discovery and start, workspace
//! materialisation, the file watcher and snapshots, and the offline journal.

pub mod changes;
pub mod claims;
pub mod connection;
pub mod daemon;
pub mod events;
pub mod history;
mod import;
pub mod receipts;
pub mod reviews;
pub mod snapshot;
pub mod workspace;

pub use connection::Client;
pub use daemon::connect_or_start;
pub use rosen_protocol as protocol;
pub use rosen_protocol::Endpoint;

use rosen_protocol::ErrorBody;

/// Errors a client operation can return.
#[derive(Debug, thiserror::Error)]
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
    /// A workspace could not be scanned or snapshotted.
    #[error(transparent)]
    Snapshot(#[from] snapshot::SnapshotError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result type for client operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Runs a client operation by its command path, for example `change new`.
///
/// Every operation without a typed method yet reports
/// [`Error::NotImplemented`]; each feature replaces its operation with a typed
/// method as it lands.
pub fn run_operation(operation: &str) -> Result<()> {
    Err(Error::NotImplemented {
        operation: operation.to_owned(),
    })
}
