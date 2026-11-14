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
//! cursor moves only when it acknowledges what it has handled, and an event
//! whose write fails is queued and retried in order rather than dropped.
//! A producer that must not lose its event to a stop between its commit and
//! the event's write stages it with the commit; see [`outbox`].

pub mod audience;
mod channel;
mod landing_sink;
mod outbox;
mod pending;
mod store;

pub use audience::Concern;
pub use channel::ChannelLine;
pub use landing_sink::LandingEvents;
pub use outbox::{ensure_schema as ensure_outbox_schema, stage, Staged};
pub use store::Page;

use crate::ServerState;
use pending::Pending;
use rosen_core::AgentId;
use rosen_protocol::{ErrorBody, ErrorCode, Event, EventCursor, EventKind, Outcome, Response};
use rusqlite::{Connection, OpenFlags};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use tokio::sync::watch;

/// Why the log could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("event log: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("event log: {0}")]
    Io(#[from] std::io::Error),
}

/// Finds the owners of live changes whose edits overlap a change, by the
/// SPEC §2 overlap rule. The server installs it: the log cannot compute it
/// from its own tables, because edits live in the changes' trees.
pub type OverlapOwners = dyn Fn(&str) -> Result<Vec<AgentId>, String> + Send + Sync;

pub struct EventLog {
    inner: Arc<Inner>,
    overlap_owners: OnceLock<Box<OverlapOwners>>,
}

/// The log's state, shared with the thread that retries failed writes.
pub(crate) struct Inner {
    /// `None` keeps the log in memory, for tests.
    db_path: Option<PathBuf>,
    slot: Mutex<Slot>,
    latest: watch::Sender<u64>,
    /// True while a retry thread owns the queue's unstored events.
    retrying: AtomicBool,
}

struct Slot {
    /// Opened on first use, like the change registry's, so a server that
    /// never publishes or reads an event never creates the database.
    conn: Option<Connection>,
    /// Events whose write failed, oldest first. Every new event queues
    /// behind them so the log keeps publish order.
    queue: VecDeque<Pending>,
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
        let slot = Slot {
            conn: None,
            queue: VecDeque::new(),
        };
        let inner = Inner {
            db_path,
            slot: Mutex::new(slot),
            latest: watch::Sender::new(0),
            retrying: AtomicBool::new(false),
        };
        Self {
            inner: Arc::new(inner),
            overlap_owners: OnceLock::new(),
        }
    }

    /// Installs how to find the owners of changes whose edits overlap an
    /// event's change. Only the first call takes effect.
    pub fn set_overlap_owners(&self, find: Box<OverlapOwners>) {
        let _ = self.overlap_owners.set(find);
    }

    /// Publishes an event about `change` to the agents it concerns, or to
    /// every subscriber when `change` is `None`. Returns its sequence number,
    /// or 0 when its write failed: it is then queued and stored, in order,
    /// by a retry.
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
        self.enqueue_about(kind, change, detail, concern, None)
    }

    /// Moves every staged event into the log, oldest first (see [`outbox`]).
    /// Call it after the transaction that staged them commits, with no
    /// registry connection held: finding an event's audience reads it. A
    /// write that fails stays queued and retried; a stop before then leaves
    /// the event staged, and the next start delivers it.
    pub fn deliver_staged(&self) {
        let staged = match self.with_db(true, |conn| Ok(outbox::staged(conn)?)) {
            Ok(staged) => staged.unwrap_or_default(),
            Err(err) => {
                eprintln!("rosen-server: staged events wait for the next delivery: {err}");
                return;
            }
        };
        for (id, event) in staged {
            let Staged {
                kind,
                change,
                detail,
                concern,
            } = event;
            self.enqueue_about(kind, change, detail, &concern, Some(id));
        }
    }

    /// Queues an event about `change` with its audience, moved from outbox
    /// row `outbox` when it was staged.
    fn enqueue_about(
        &self,
        kind: EventKind,
        change: Option<String>,
        detail: serde_json::Value,
        concern: &Concern,
        outbox: Option<i64>,
    ) -> u64 {
        // Found before taking the log's lock: it reads the change registry
        // and trees, and must not hold up the log's readers meanwhile.
        let concern = change
            .as_deref()
            .and_then(|id| self.with_overlapping_owners(id, concern));
        let audience = |conn: Option<&Connection>| match (&change, &concern, conn) {
            (Some(id), Some(concern), Some(conn)) => audience_or_everyone(conn, id, concern),
            _ => None,
        };
        self.enqueue(kind, change.clone(), detail, audience, outbox)
    }

    /// Both producers use this queue so a failed write cannot let a channel
    /// message overtake the Rosen event that preceded it (or vice versa).
    fn enqueue(
        &self,
        kind: EventKind,
        change: Option<String>,
        detail: serde_json::Value,
        audience: impl FnOnce(Option<&Connection>) -> Option<std::collections::BTreeSet<String>>,
        outbox: Option<i64>,
    ) -> u64 {
        let inner = &self.inner;
        inner.with_slot(|slot| {
            let opened = inner.connection(&mut slot.conn, true);
            // The audience is fixed now, from the state the publisher just
            // committed, even if the write itself is retried later.
            let audience = audience(opened.as_ref().ok().and_then(|conn| conn.as_deref()));
            slot.queue.push_back(Pending {
                kind,
                change,
                detail,
                audience,
                outbox,
            });
            let stored = match opened {
                Ok(Some(conn)) => pending::flush(inner, conn, &mut slot.queue),
                Ok(None) => Ok(0),
                Err(err) => Err(err),
            };
            match stored {
                Ok(seq) if slot.queue.is_empty() => seq,
                outcome => {
                    if let Err(err) = outcome {
                        eprintln!("rosen-server: queued a {kind:?} event to retry: {err}");
                    }
                    pending::ensure_retrying(inner);
                    0
                }
            }
        })
    }

    /// `concern` plus the owners of changes whose edits overlap `change`, or
    /// `None` (broadcast) when they cannot be found: delivery is at least
    /// once, so an event goes to too many subscribers rather than too few.
    fn with_overlapping_owners(&self, change: &str, concern: &Concern) -> Option<Concern> {
        let mut concern = concern.clone();
        if let Some(find) = self.overlap_owners.get() {
            match find(change) {
                Ok(owners) => concern.agents.extend(owners),
                Err(err) => {
                    eprintln!("rosen-server: broadcasting an event about {change}: {err}");
                    return None;
                }
            }
        }
        Some(concern)
    }

    /// True when this exact event is stored or queued to be.
    /// An unreadable log answers false, so the caller publishes again: a
    /// repeat is allowed (delivery is at least once), a lost event is not.
    pub fn holds(&self, kind: EventKind, change: &str, detail: &serde_json::Value) -> bool {
        let inner = &self.inner;
        inner.with_slot(|slot| {
            let queued = slot.queue.iter().any(|pending| {
                pending.kind == kind
                    && pending.change.as_deref() == Some(change)
                    && pending.detail == *detail
            });
            queued
                || matches!(inner.connection(&mut slot.conn, false),
                    Ok(Some(conn)) if store::holds(conn, kind, change, detail).unwrap_or(false))
        })
    }

    /// Every event after `cursor`, oldest first.
    pub fn after(&self, cursor: u64) -> Vec<Event> {
        match self.page(cursor, None, u64::MAX) {
            Ok(page) => page.events,
            Err(err) => {
                eprintln!("rosen-server: could not read events: {err}");
                Vec::new()
            }
        }
    }

    /// Up to `limit` rows after `cursor` for one subscriber: broadcasts and
    /// events that concern it, or every event when `subscriber` is `None`.
    pub fn page(
        &self,
        cursor: u64,
        subscriber: Option<&AgentId>,
        limit: u64,
    ) -> Result<Page, EventError> {
        let agent = subscriber.map(AgentId::as_str);
        let page = self.with_db(false, |conn| Ok(store::page(conn, cursor, agent, limit)?))?;
        Ok(page.unwrap_or(Page {
            scanned_to: cursor,
            ..Page::default()
        }))
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

    /// True when an event of `kind` is stored after `cursor`. Reads SQLite:
    /// call it on the blocking pool.
    pub fn holds_kind_after(&self, cursor: u64, kind: EventKind) -> Result<bool, EventError> {
        let found = self.with_db(false, |conn| {
            Ok(store::holds_kind_after(conn, cursor, kind)?)
        })?;
        Ok(found.unwrap_or(false))
    }

    /// The newest sequence number announced so far, without touching the
    /// database or the log's lock, so async code can read it. It equals
    /// [`EventLog::last_seq`] once the log has been opened; the server opens
    /// it at startup.
    pub fn announced_seq(&self) -> u64 {
        *self.inner.latest.borrow()
    }

    /// The newest stored sequence number, 0 when the log is empty. Opens
    /// the database: call it on the blocking pool.
    pub fn last_seq(&self) -> u64 {
        // Opening reads the stored sequence, so a restarted server reports
        // the log's real tip rather than 0.
        if let Err(err) = self.with_db(false, |_| Ok(())) {
            eprintln!("rosen-server: could not open the event log: {err}");
        }
        *self.inner.latest.borrow()
    }

    /// A receiver that changes whenever a new event is stored.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.inner.latest.subscribe()
    }

    /// Runs `work` on the log's connection. Returns `Ok(None)` without
    /// running it when `create` is false and no database exists yet: then
    /// there is nothing to read, and reading must not create one.
    fn with_db<T>(
        &self,
        create: bool,
        work: impl FnOnce(&mut Connection) -> Result<T, EventError>,
    ) -> Result<Option<T>, EventError> {
        let inner = &self.inner;
        inner.with_slot(|slot| match inner.connection(&mut slot.conn, create)? {
            Some(conn) => work(conn).map(Some),
            None => Ok(None),
        })
    }
}

impl Inner {
    fn with_slot<T>(&self, work: impl FnOnce(&mut Slot) -> T) -> T {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        work(&mut slot)
    }

    /// The open connection, opening it first if needed. `Ok(None)` when
    /// `create` is false and no database exists yet.
    fn connection<'a>(
        &self,
        conn: &'a mut Option<Connection>,
        create: bool,
    ) -> Result<Option<&'a mut Connection>, EventError> {
        if conn.is_none() {
            *conn = self.connect(create)?;
        }
        Ok(conn.as_mut())
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
