//! The project's AIRC channel, held by the server (SPEC R13): presence,
//! piece claims, agent messages and the two ledgers.
//!
//! Every machine's Velocity calls these through `rosen-client`; a cache
//! daemon forwards them, so the server is the channel's only copy and no
//! two machines can see different claim holders.
//!
//! Channel state lives in the metadata store and is read and written on the
//! event log's connection (R9, R13 "One log"). Each write is one IMMEDIATE
//! transaction that also appends its `channel` event lines, so two claims on
//! one piece are serialised, the second sees the first, and a committed
//! change never lacks its event.

mod ledgers;
mod messages;
pub mod pieces;
mod presence;
mod schema;

pub use schema::ensure_schema;

use crate::events::{ChannelLine, EventError, EventLog};
use crate::ServerState;
use rosen_core::AgentId;
use rosen_protocol::channel::{ChannelAnswer, ChannelEvent, ChannelRequest};
use rosen_protocol::{ErrorBody, ErrorCode, Outcome, Response};
use rusqlite::Transaction;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// The most rows one read returns; callers page with the returned cursor.
pub const PAGE_LIMIT: u64 = 500;

/// Why a channel operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("channel store: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("channel store: {0}")]
    Event(#[from] EventError),
    #[error("member {0} has not joined the channel")]
    NotJoined(String),
    #[error("no member has the nick {0:?}")]
    UnknownNick(String),
    #[error("member {0} is quarantined: its piece claims are frozen")]
    Frozen(String),
    #[error("nick {nick:?} is in use by member {member}")]
    NickTaken { nick: String, member: String },
    #[error("{0}")]
    Invalid(String),
}

impl ChannelError {
    fn code(&self) -> ErrorCode {
        match self {
            ChannelError::Sql(_) | ChannelError::Event(_) => ErrorCode::Internal,
            ChannelError::NotJoined(_) | ChannelError::UnknownNick(_) => ErrorCode::NotFound,
            ChannelError::Frozen(_) | ChannelError::NickTaken { .. } => ErrorCode::Refused,
            ChannelError::Invalid(_) => ErrorCode::InvalidArgument,
        }
    }
}

/// What a write returns: its answer and the channel lines to append with it.
type Written = (ChannelAnswer, Vec<ChannelLine>);

/// Answers [`rosen_protocol::Request::Channel`] on the blocking pool, as
/// every SQLite call is.
pub async fn handle(state: &Arc<ServerState>, call: ChannelRequest) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || run(&state.events, call)).await {
        Ok(Ok(answer)) => Outcome::Ok(Response::Channel(answer)),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(err.code(), err.to_string())),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

/// Runs one channel operation to completion on the log's connection.
pub fn run(events: &EventLog, call: ChannelRequest) -> Result<ChannelAnswer, ChannelError> {
    use ChannelRequest as C;
    match call {
        C::Presence => events.read_channel(|c| Ok(ChannelAnswer::Members(presence::list(c)?))),
        C::Pieces => events.read_channel(|c| Ok(ChannelAnswer::Pieces(pieces::list(c)?))),
        C::Messages { reader, after } => events.read_channel(|c| {
            messages::read(c, reader.as_deref(), after).map(ChannelAnswer::Messages)
        }),
        C::ReadLedger { ledger, after } => {
            events.read_channel(|c| ledgers::read(c, ledger, after).map(ChannelAnswer::Entries))
        }
        write => events.commit_channel_lines(|tx| apply(tx, write)),
    }
}

fn apply(tx: &Transaction, call: ChannelRequest) -> Result<Written, ChannelError> {
    use ChannelRequest as C;
    match call {
        C::Join {
            member,
            nick,
            machine,
        } => presence::join(tx, &member, &nick, &machine),
        C::Leave { member } => presence::leave(tx, &member),
        C::SetMode { member, mode } => presence::set_mode(tx, &member, mode),
        C::ClaimPiece {
            member,
            piece,
            task,
            branch,
        } => pieces::claim(tx, &member, piece.as_deref(), &task, &branch),
        C::ReleasePiece { member, piece } => pieces::release(tx, &member, piece.as_deref()),
        C::Send(message) => messages::send(tx, &message),
        C::ReleaseHeld { member } => messages::release_held(tx, &member),
        C::Append(entry) => ledgers::append(tx, &entry),
        C::Presence | C::Pieces | C::Messages { .. } | C::ReadLedger { .. } => Err(
            ChannelError::Invalid("a read is not a channel write".into()),
        ),
    }
}

/// A line for the project view and the given agents, by nick.
fn line(event: &ChannelEvent, nicks: &[&str]) -> Result<ChannelLine, ChannelError> {
    let detail =
        serde_json::to_value(event).map_err(|err| ChannelError::Invalid(err.to_string()))?;
    let recipients = nicks
        .iter()
        .map(|nick| AgentId::new(*nick).map_err(|err| ChannelError::Invalid(err.to_string())))
        .collect::<Result<_, _>>()?;
    Ok(ChannelLine { detail, recipients })
}

/// A line only the project view receives.
fn project_line(event: &ChannelEvent) -> Result<ChannelLine, ChannelError> {
    line(event, &[])
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn to_seq(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn from_seq(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Reads a stored enum column. A value this build does not know fails the
/// read rather than being guessed.
fn enum_from<T>(text: String, parse: fn(&str) -> Option<T>) -> rusqlite::Result<T> {
    parse(&text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown channel value {text:?}").into(),
        )
    })
}
