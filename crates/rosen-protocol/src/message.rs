//! Wire messages exchanged between a Rosen client and the server.
//!
//! Every connection starts with a handshake: the client sends [`ClientFrame::Hello`]
//! listing the protocol versions it speaks, and the server answers with
//! [`ServerFrame::Welcome`] naming the version both sides will use, or
//! [`ServerFrame::Reject`]. After that the client sends requests tagged with an id
//! it chooses, and the server answers each with a response carrying the same id.
//! Responses may arrive out of order, and event frames are interleaved with them.

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
    },
    /// A request; the server answers with a [`ServerFrame::Response`] with the same id.
    Request { id: u64, request: Request },
    /// Starts this connection's event stream, delivering every event after
    /// `after` (or every retained event when `after` is `None`).
    Subscribe { after: Option<u64> },
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
}

/// Successful results, one per [`Request`] variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Status(ServerStatus),
    ShuttingDown,
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
    pub uptime_ms: u64,
    pub open_connections: u64,
    /// The sequence number of the newest event, 0 when there is none.
    pub last_event: u64,
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
}

/// The event types of SPEC R9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Snapshot,
    Landed,
    Rebased,
    Conflict,
    Updated,
    Review,
    Claim,
    Released,
    Green,
    Reverted,
    Mirror,
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
