//! The project's AIRC channel, held by the server (SPEC R13): presence,
//! piece claims, agent messages and the two ledgers.
//!
//! Every machine's Velocity calls these through `rosen-client`; a cache
//! daemon forwards them, so the server is the channel's only copy and no
//! two machines can see different claim holders. Each write is one
//! IMMEDIATE transaction, which takes SQLite's write lock before reading:
//! two claims on one piece are serialised and the second sees the first.

mod ledgers;
mod messages;
pub mod pieces;
mod presence;
mod schema;

pub use schema::ensure_schema;

use crate::ServerState;
use rosen_protocol::channel::{ChannelAnswer, ChannelRequest};
use rosen_protocol::{ErrorBody, ErrorCode, Outcome, Response};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// The most rows one read returns; callers page with the returned cursor.
pub const PAGE_LIMIT: u64 = 500;

/// Why a channel operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("channel store: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("channel store: {0}")]
    Io(#[from] std::io::Error),
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
            ChannelError::Sql(_) | ChannelError::Io(_) => ErrorCode::Internal,
            ChannelError::NotJoined(_) | ChannelError::UnknownNick(_) => ErrorCode::NotFound,
            ChannelError::Frozen(_) | ChannelError::NickTaken { .. } => ErrorCode::Refused,
            ChannelError::Invalid(_) => ErrorCode::InvalidArgument,
        }
    }
}

/// The channel's connection to the metadata store. Separate from the change
/// registry's so channel traffic never queues behind a land or a snapshot;
/// WAL and the busy timeout let the two write the same file.
pub struct ChannelService {
    /// `None` keeps the channel in memory, for tests.
    db_path: Option<PathBuf>,
    conn: Mutex<Option<Connection>>,
}

impl ChannelService {
    pub fn open(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: Some(db_path.into()),
            conn: Mutex::new(None),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            db_path: None,
            conn: Mutex::new(None),
        }
    }

    /// Runs one channel operation to completion.
    pub fn run(&self, call: ChannelRequest) -> Result<ChannelAnswer, ChannelError> {
        self.with_conn(|conn| run(conn, call))
    }

    fn with_conn<T>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, ChannelError>,
    ) -> Result<T, ChannelError> {
        let mut slot = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(self.connect()?);
        }
        work(slot.as_mut().expect("connection opened above"))
    }

    fn connect(&self) -> Result<Connection, ChannelError> {
        let Some(path) = &self.db_path else {
            let conn = Connection::open_in_memory()?;
            crate::metadata::configure(&conn)?;
            ensure_schema(&conn)?;
            return Ok(conn);
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Ok(crate::metadata::open(path)?)
    }
}

/// Answers [`rosen_protocol::Request::Channel`] on the blocking pool, as
/// every SQLite call is.
pub async fn handle(state: &Arc<ServerState>, call: ChannelRequest) -> Outcome {
    let state = Arc::clone(state);
    match tokio::task::spawn_blocking(move || state.channel.run(call)).await {
        Ok(Ok(answer)) => Outcome::Ok(Response::Channel(answer)),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(err.code(), err.to_string())),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}

fn run(conn: &mut Connection, call: ChannelRequest) -> Result<ChannelAnswer, ChannelError> {
    use ChannelRequest as C;
    Ok(match call {
        C::Join {
            member,
            nick,
            machine,
        } => ChannelAnswer::Member(presence::join(conn, &member, &nick, &machine)?),
        C::Leave { member } => ChannelAnswer::Member(presence::leave(conn, &member)?),
        C::SetMode { member, mode } => {
            ChannelAnswer::Member(presence::set_mode(conn, &member, mode)?)
        }
        C::Presence => ChannelAnswer::Members(presence::list(conn)?),
        C::ClaimPiece {
            member,
            piece,
            task,
            branch,
        } => ChannelAnswer::Piece(pieces::claim(
            conn,
            &member,
            piece.as_deref(),
            &task,
            &branch,
        )?),
        C::ReleasePiece { member, piece } => {
            ChannelAnswer::Pieces(pieces::release(conn, &member, piece.as_deref())?)
        }
        C::Pieces => ChannelAnswer::Pieces(pieces::list(conn)?),
        C::Send(message) => ChannelAnswer::Sent(messages::send(conn, &message)?),
        C::Messages { reader, after } => {
            ChannelAnswer::Messages(messages::read(conn, reader.as_deref(), after)?)
        }
        C::ReleaseHeld { member } => {
            ChannelAnswer::Delivered(messages::release_held(conn, &member)?)
        }
        C::Append(entry) => ChannelAnswer::Entry(ledgers::append(conn, &entry)?),
        C::ReadLedger { ledger, after } => {
            ChannelAnswer::Entries(ledgers::read(conn, ledger, after)?)
        }
    })
}

/// Opens a transaction that holds the write lock from its first read, so
/// the read and the write it decides cannot interleave with another one.
fn write_tx(conn: &mut Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
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
