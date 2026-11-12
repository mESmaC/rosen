//! The Rosen client library (SPEC.md D7, §3).
//!
//! Velocity links this crate directly, and the `rosen` CLI is a thin layer
//! over it. It covers daemon discovery and start, workspace
//! materialisation, the file watcher and snapshots, and the offline journal.

/// Errors a client operation can return.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The operation exists in the CLI and API surface but has no
    /// implementation in this build yet.
    #[error("`{operation}` is not implemented yet")]
    NotImplemented { operation: String },
}

/// Result type for client operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Runs a client operation by its command path, for example `change new`.
///
/// Every operation currently reports [`Error::NotImplemented`]; each feature
/// replaces its operation with a typed method as it lands.
pub fn run_operation(operation: &str) -> Result<()> {
    Err(Error::NotImplemented {
        operation: operation.to_owned(),
    })
}
