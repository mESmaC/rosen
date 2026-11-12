//! `rosen-client`: the library Velocity links and the `rosen` CLI wraps (SPEC D7).

pub mod connection;
pub mod daemon;

pub use connection::Client;
pub use daemon::connect_or_start;
pub use rosen_protocol as protocol;
pub use rosen_protocol::Endpoint;

use rosen_protocol::ErrorBody;
use std::fmt;

/// Why a client call failed.
#[derive(Debug)]
pub enum ClientError {
    /// No server listens on the endpoint.
    NotRunning(Endpoint),
    /// The daemon could not be launched, or did not come up in time.
    Start(String),
    /// The server refused the handshake, usually a protocol version mismatch.
    Rejected {
        reason: String,
        supported: Vec<u32>,
    },
    /// The server answered the request with an error.
    Server(ErrorBody),
    /// The connection closed before the answer arrived.
    Disconnected,
    /// The server sent something the protocol does not allow here.
    Unexpected(String),
    Io(std::io::Error),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning(endpoint) => write!(f, "no rosen server is running on {endpoint}"),
            Self::Start(reason) => write!(f, "could not start rosen-server: {reason}"),
            Self::Rejected { reason, supported } => write!(
                f,
                "the server refused the connection: {reason} (it speaks protocol versions {supported:?})"
            ),
            Self::Server(error) => write!(f, "server error: {error}"),
            Self::Disconnected => f.write_str("the connection to the server closed"),
            Self::Unexpected(what) => write!(f, "protocol error: {what}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ClientError {}
