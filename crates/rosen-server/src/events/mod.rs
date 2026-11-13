//! The server's event log and delivery to subscribers (SPEC R9).
//!
//! Every event is stored in the metadata store with a sequence number, which
//! is the cursor a subscriber resumes from. The log, its sequence and each
//! subscriber's acknowledged cursor survive a server restart.
//!
//! An event about a change is targeted: when it is published, [`audience`]
//! records which agents it concerns, and a subscriber that names its agent
//! receives only those events plus broadcasts. A subscriber that names no
//! agent is the project view and receives every event.
//!
//! Subscribers read the log from their cursor, then sleep on a watch channel
//! until a newer sequence number is published, so delivery never depends on a
//! bounded channel keeping up. Delivery is at least once: a subscriber's
//! cursor moves only when it acknowledges what it has handled.

pub mod audience;
mod landing_sink;
mod store;

pub use audience::Concern;
pub use landing_sink::LandingEvents;

use crate::ServerState;
use rosen_core::AgentId;
use rosen_protocol::{ErrorBody, ErrorCode, Event, EventCursor, EventKind, Outcome, Response};
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::watch;

/// Why the log could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("event log: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("event log: {0}")]
    Io(#[from] std::io::Error),
}

pub struct EventLog {
    /// `None` keeps the log in memory, for tests.
    db_path: Option<PathBuf>,
    /// Opened on first use, like the change registry's, so a server that
    /// never publishes or reads an event never creates the database.
    db: Mutex<Option<Connection>>,
    latest: watch::Sender<u64>,
}

impl EventLog {
    /// A log stored in the metadata database at `db_path`.
    pub fn open(db_path: impl Into<PathBuf>) -> Self {
        Self::with_path(Some(db_path.into()))
    }

    /// A log that lives only as long as the server, for tests.
    pub fn in_memory() -> Self {
        Self::with_path(None)
    }

    fn with_path(db_path: Option<PathBuf>) -> Self {
        Self {
            db_path,
            db: Mutex::new(None),
            latest: watch::Sender::new(0),
        }
    }

    /// Publishes an event about `change` to the agents it concerns, or to
    /// every subscriber when `change` is `None`. Returns its sequence number,
    /// or 0 when it could not be stored (the failure is logged).
    pub fn publish(
        &self,
        kind: EventKind,
        change: Option<String>,
        detail: serde_json::Value,
    ) -> u64 {
        self.publish_to(kind, change, detail, &Concern::default())
    }

    /// [`EventLog::publish`], with agents or paths the change model does not
    /// record that the event also concerns.
    pub fn publish_to(
        &self,
        kind: EventKind,
        change: Option<String>,
        detail: serde_json::Value,
        concern: &Concern,
    ) -> u64 {
        let stored = self.with_db(true, |conn| {
            let audience = match &change {
                Some(id) => audience_or_everyone(conn, id, concern),
                None => None,
            };
            let seq = store::append(conn, kind, change.as_deref(), &detail, audience.as_ref())?;
            // Publish while holding the lock so subscribers see sequence
            // numbers in order.
            self.latest.send_replace(seq);
            Ok(seq)
        });
        match stored {
            Ok(seq) => seq.unwrap_or(0),
            Err(err) => {
                eprintln!("rosen-server: could not store a {kind:?} event: {err}");
                0
            }
        }
    }

    /// Every event after `cursor`, oldest first.
    pub fn after(&self, cursor: u64) -> Vec<Event> {
        self.page(cursor, None, u64::MAX).unwrap_or_else(|err| {
            eprintln!("rosen-server: could not read events: {err}");
            Vec::new()
        })
    }

    /// Up to `limit` events after `cursor` for one subscriber: broadcasts
    /// and events that concern it, or every event when `subscriber` is `None`.
    pub fn page(
        &self,
        cursor: u64,
        subscriber: Option<&AgentId>,
        limit: u64,
    ) -> Result<Vec<Event>, EventError> {
        let agent = subscriber.map(AgentId::as_str);
        let events = self.with_db(false, |conn| Ok(store::page(conn, cursor, agent, limit)?))?;
        Ok(events.unwrap_or_default())
    }

    /// Where the subscriber resumes when it names no cursor: after the last
    /// event it acknowledged, if it ever did.
    pub fn cursor(&self, subscriber: &AgentId) -> Result<Option<u64>, EventError> {
        let stored = self.with_db(false, |conn| Ok(store::cursor(conn, subscriber.as_str())?))?;
        Ok(stored.flatten())
    }

    /// Records that the subscriber has handled every event up to `seq`.
    /// Returns the cursor now stored, which never moves backwards.
    pub fn acknowledge(&self, subscriber: &AgentId, seq: u64) -> Result<u64, EventError> {
        let stored = self.with_db(true, |conn| {
            Ok(store::acknowledge(conn, subscriber.as_str(), seq)?)
        })?;
        Ok(stored.unwrap_or(0))
    }

    /// The newest sequence number, 0 when the log is empty.
    pub fn last_seq(&self) -> u64 {
        // Opening reads the stored sequence, so a restarted server reports
        // the log's real tip rather than 0.
        if let Err(err) = self.with_db(false, |_| Ok(())) {
            eprintln!("rosen-server: could not open the event log: {err}");
        }
        *self.latest.borrow()
    }

    /// A receiver that changes whenever a new event is published.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.latest.subscribe()
    }

    /// Runs `work` on the log's connection. Returns `Ok(None)` without
    /// running it when `create` is false and no database exists yet: then
    /// there is nothing to read, and reading must not create one.
    fn with_db<T>(
        &self,
        create: bool,
        work: impl FnOnce(&mut Connection) -> Result<T, EventError>,
    ) -> Result<Option<T>, EventError> {
        let mut slot = self.db.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            match self.connect(create)? {
                Some(conn) => *slot = Some(conn),
                None => return Ok(None),
            }
        }
        work(slot.as_mut().expect("connection opened above")).map(Some)
    }

    fn connect(&self, create: bool) -> Result<Option<Connection>, EventError> {
        let conn = match &self.db_path {
            None => Connection::open_in_memory()?,
            Some(path) if !create && !path.exists() => return Ok(None),
            Some(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX;
                Connection::open_with_flags(path, flags)?
            }
        };
        crate::metadata::configure(&conn)?;
        crate::metadata::ensure_schema(&conn)?;
        store::ensure_schema(&conn)?;
        let tip = store::last_seq(&conn)?;
        self.latest.send_if_modified(|latest| {
            let newer = tip > *latest;
            *latest = (*latest).max(tip);
            newer
        });
        Ok(Some(conn))
    }
}

/// The agents an event about `change` concerns, or `None` (everyone) when
/// they cannot be read: delivery is at least once, so an event goes to too
/// many subscribers rather than too few.
fn audience_or_everyone(
    conn: &Connection,
    change: &str,
    concern: &Concern,
) -> Option<std::collections::BTreeSet<String>> {
    match audience::concerned(conn, change, concern) {
        Ok(agents) => Some(agents),
        Err(err) => {
            eprintln!("rosen-server: broadcasting an event about {change}: {err}");
            None
        }
    }
}

/// Answers [`rosen_protocol::Request::AckEvents`] on the blocking pool, as
/// every SQLite call is.
pub async fn acknowledge(state: &Arc<ServerState>, subscriber: AgentId, seq: u64) -> Outcome {
    let state = Arc::clone(state);
    let work = move || {
        let newest = state.events.last_seq();
        if seq > newest {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgument,
                format!("cannot acknowledge event {seq}: the newest event is {newest}"),
            ));
        }
        let cursor = state
            .events
            .acknowledge(&subscriber, seq)
            .map_err(|err| ErrorBody::new(ErrorCode::Internal, err.to_string()))?;
        Ok(Response::EventsAcked(EventCursor { subscriber, cursor }))
    };
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(error)) => Outcome::Error(error),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}
