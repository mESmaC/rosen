//! Runtime health of the Collapse state feed, ledger push and action relay
//! (R9, R10, #434).
//!
//! Each background task updates its slot in [`Tracker`] as it starts,
//! succeeds, fails or stops. `Request::Status` combines those live fields with
//! the durable cursors and unsent backlogs in SQLite so a client that connects
//! later sees the current link health without reading the event log.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rosen_protocol::{CollapseHealth, FeedError, FeedHealth, FeedState, LedgerCursors};
use rusqlite::{params, Connection, OptionalExtension};

use crate::ServerState;

/// One of the three Collapse links tracked in [`Tracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    StateFeed = 0,
    LedgerPush = 1,
    Relay = 2,
}

/// Live in-memory state of all three Collapse links on one server.
#[derive(Debug, Default)]
pub struct Tracker {
    feeds: [Arc<Mutex<FeedSlot>>; 3],
}

#[derive(Debug, Clone, Default)]
struct FeedSlot {
    /// The endpoint the tracked fields apply to. When a land changes the
    /// configured endpoint, stale errors or stops for the old one are ignored.
    endpoint: Option<String>,
    in_flight: bool,
    has_pending: bool,
    stopped: bool,
    no_credential: bool,
    /// Highest event sequence confirmed clean in memory by `state_push`,
    /// avoiding false backlog counts for ignored events between sends.
    clean_scanned: Option<u64>,
    last_success_at: Option<i64>,
    last_error: Option<FeedError>,
    next_retry_at: Option<i64>,
}

impl FeedSlot {
    /// Resets endpoint-specific state when a land moves the target URL.
    fn select_endpoint(&mut self, endpoint: &str) {
        if self.endpoint.as_deref() != Some(endpoint) {
            *self = Self {
                endpoint: Some(endpoint.to_owned()),
                ..Self::default()
            };
        }
    }
}

/// Marks a feed in flight until the guard drops.
pub struct InFlightGuard {
    slot: Arc<Mutex<FeedSlot>>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut slot = lock(&self.slot);
        slot.in_flight = false;
    }
}

impl Tracker {
    /// Marks `feed` as actively sending/polling `endpoint` until dropped.
    pub fn begin(&self, feed: Feed, endpoint: &str) -> InFlightGuard {
        let arc = Arc::clone(&self.feeds[feed as usize]);
        {
            let mut slot = lock(&arc);
            slot.select_endpoint(endpoint);
            slot.in_flight = true;
        }
        InFlightGuard { slot: arc }
    }

    /// Records a successful HTTP exchange with `endpoint`.
    pub fn record_success(&self, feed: Feed, endpoint: &str) {
        let mut slot = lock(&self.feeds[feed as usize]);
        slot.select_endpoint(endpoint);
        slot.stopped = false;
        slot.no_credential = false;
        slot.last_success_at = Some(now_millis());
        slot.last_error = None;
        slot.next_retry_at = None;
    }

    /// Records that `feed` is idle with nothing left to send for `endpoint`,
    /// clearing any earlier failure state without fabricating a contact time.
    pub fn record_idle(&self, feed: Feed, endpoint: &str, clean_scanned: Option<u64>) {
        let mut slot = lock(&self.feeds[feed as usize]);
        slot.select_endpoint(endpoint);
        slot.has_pending = false;
        slot.stopped = false;
        slot.no_credential = false;
        slot.last_error = None;
        slot.next_retry_at = None;
        if let Some(seq) = clean_scanned {
            slot.clean_scanned = Some(slot.clean_scanned.map_or(seq, |prev| prev.max(seq)));
        }
    }

    /// Records whether `feed` has unsent work waiting (e.g., settle/debounce).
    pub fn set_pending(
        &self,
        feed: Feed,
        endpoint: &str,
        pending: bool,
        clean_scanned: Option<u64>,
    ) {
        let mut slot = lock(&self.feeds[feed as usize]);
        slot.select_endpoint(endpoint);
        slot.has_pending = pending;
        if !pending {
            if let Some(seq) = clean_scanned {
                slot.clean_scanned = Some(slot.clean_scanned.map_or(seq, |prev| prev.max(seq)));
            }
        }
    }

    /// Records a retried failure (or missing HTTPS credential) for `feed`.
    pub fn record_failure(
        &self,
        feed: Feed,
        endpoint: Option<&str>,
        status: Option<u16>,
        message: &str,
        retry_in: Duration,
        no_credential: bool,
    ) {
        let now = now_millis();
        let delay = i64::try_from(retry_in.as_millis()).unwrap_or(i64::MAX);
        let mut slot = lock(&self.feeds[feed as usize]);
        if let Some(endpoint) = endpoint {
            slot.select_endpoint(endpoint);
        }
        slot.stopped = false;
        slot.no_credential = no_credential;
        slot.last_error = Some(FeedError {
            http_status: status,
            message: message.to_owned(),
            at: now,
        });
        slot.next_retry_at = Some(now.saturating_add(delay));
    }

    /// Records HTTP 410 (archived or deleted) for `feed`, which stops retries.
    pub fn record_stopped(&self, feed: Feed, endpoint: &str, message: &str) {
        let mut slot = lock(&self.feeds[feed as usize]);
        slot.select_endpoint(endpoint);
        slot.has_pending = false;
        slot.stopped = true;
        slot.no_credential = false;
        slot.last_error = Some(FeedError {
            http_status: Some(410),
            message: message.to_owned(),
            at: now_millis(),
        });
        slot.next_retry_at = None;
    }

    fn slot_for(&self, feed: Feed, endpoint: &str) -> FeedSlot {
        let slot = lock(&self.feeds[feed as usize]);
        if slot.endpoint.as_deref() == Some(endpoint) {
            slot.clone()
        } else {
            FeedSlot::default()
        }
    }
}

/// Computes `ServerStatus.collapse`, or `None` when the base names no
/// `collapse` root.
pub async fn snapshot(state: &Arc<ServerState>) -> Option<CollapseHealth> {
    if !state.config.metadata_path().is_file() {
        return None;
    }
    let state = Arc::clone(state);
    tokio::task::spawn_blocking(move || read_snapshot(&state))
        .await
        .ok()
        .flatten()
}

fn read_snapshot(state: &ServerState) -> Option<CollapseHealth> {
    let (_, resolved) = state.project_config.at_base().ok()??;
    let config = resolved.config;
    let root = config.collapse_url("")?.trim_end_matches('/').to_owned();
    let state_endpoint = config.state_feed_url()?;
    let ledger_endpoint = config.ledger_push_url()?;
    let is_owner = crate::mirror::owner::is_owner(&state.config.data_dir);
    let conn = crate::metadata::open(&state.config.metadata_path()).ok()?;

    let state_feed = state_feed_health(&conn, &state.collapse, &state_endpoint, is_owner);
    let ledger_push = ledger_push_health(&conn, &state.collapse, &ledger_endpoint, is_owner);
    let relay = relay_health(&conn, &state.collapse, &root, is_owner);
    Some(CollapseHealth {
        root,
        state_feed,
        ledger_push,
        relay,
    })
}

fn state_feed_health(
    conn: &Connection,
    tracker: &Tracker,
    endpoint: &str,
    is_owner: bool,
) -> FeedHealth {
    let tracked = tracker.slot_for(Feed::StateFeed, endpoint);
    let row = read_state_cursor(conn, endpoint).unwrap_or(None);
    let cursor = row.map(|(_, delivered)| delivered);
    let scanned = row
        .map_or(0, |(scanned, _)| scanned)
        .max(tracked.clean_scanned.unwrap_or(0));
    let backlog = count_unsent_events(conn, scanned).unwrap_or(0);
    build_health(is_owner, &tracked, cursor, None, None, backlog)
}

fn ledger_push_health(
    conn: &Connection,
    tracker: &Tracker,
    endpoint: &str,
    is_owner: bool,
) -> FeedHealth {
    let tracked = tracker.slot_for(Feed::LedgerPush, endpoint);
    let (present, cursors, scanned) = read_ledger_cursors(conn, endpoint).unwrap_or_default();
    let cursor = (present || tracked.last_success_at.is_some())
        .then(|| cursors.discovery.max(cursors.past_work));
    let backlog = count_unsent_ledger(conn, scanned).unwrap_or(0);
    build_health(is_owner, &tracked, cursor, Some(cursors), None, backlog)
}

fn relay_health(conn: &Connection, tracker: &Tracker, root: &str, is_owner: bool) -> FeedHealth {
    let tracked = tracker.slot_for(Feed::Relay, root);
    let row = read_relay_cursor(conn, root).unwrap_or(None);
    let cursor = row.map(|(_, after)| after);
    let generation = row.and_then(|(gen, _)| gen);
    let backlog = count_unposted_relay(conn, root).unwrap_or(0);
    build_health(is_owner, &tracked, cursor, None, generation, backlog)
}

fn build_health(
    is_owner: bool,
    tracked: &FeedSlot,
    cursor: Option<u64>,
    ledger_cursors: Option<LedgerCursors>,
    doorbell_generation: Option<i64>,
    backlog: u64,
) -> FeedHealth {
    let (state, last_error, next_retry_at) = if !is_owner {
        (FeedState::NotOwner, None, None)
    } else if tracked.stopped {
        (FeedState::Stopped, tracked.last_error.clone(), None)
    } else if tracked.no_credential {
        (
            FeedState::NoCredential,
            tracked.last_error.clone(),
            tracked.next_retry_at,
        )
    } else if tracked.last_error.is_some() {
        (
            FeedState::Failing,
            tracked.last_error.clone(),
            tracked.next_retry_at,
        )
    } else if tracked.in_flight || tracked.has_pending || backlog > 0 {
        (FeedState::Sending, None, None)
    } else {
        (FeedState::Current, None, None)
    };
    FeedHealth {
        state,
        last_success_at: tracked.last_success_at,
        cursor,
        ledger_cursors,
        doorbell_generation,
        backlog,
        last_error,
        next_retry_at,
    }
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )
}

fn read_state_cursor(conn: &Connection, endpoint: &str) -> rusqlite::Result<Option<(u64, u64)>> {
    if !table_exists(conn, "state_push")? {
        return Ok(None);
    }
    conn.query_row(
        "SELECT scanned, delivered FROM state_push WHERE endpoint = ?1",
        [endpoint],
        |row| Ok((to_u64(row.get::<_, i64>(0)?), to_u64(row.get::<_, i64>(1)?))),
    )
    .optional()
}

fn count_unsent_events(conn: &Connection, scanned: u64) -> rusqlite::Result<u64> {
    if !table_exists(conn, "events")? {
        return Ok(0);
    }
    let from = i64::try_from(scanned).unwrap_or(i64::MAX);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events
         WHERE seq > ?1 AND kind NOT IN ('state_push', 'ledger_push')",
        [from],
        |row| row.get(0),
    )?;
    Ok(to_u64(count))
}

fn read_ledger_cursors(
    conn: &Connection,
    endpoint: &str,
) -> rusqlite::Result<(bool, LedgerCursors, (u64, u64))> {
    if !table_exists(conn, "ledger_push")? {
        return Ok((false, LedgerCursors::default(), (0, 0)));
    }
    let mut stmt =
        conn.prepare("SELECT ledger, scanned, delivered FROM ledger_push WHERE endpoint = ?1")?;
    let rows = stmt.query_map([endpoint], |row| {
        Ok((
            row.get::<_, String>(0)?,
            to_u64(row.get::<_, i64>(1)?),
            to_u64(row.get::<_, i64>(2)?),
        ))
    })?;
    let mut present = false;
    let mut delivered = LedgerCursors::default();
    let mut scanned = (0u64, 0u64);
    for row in rows {
        let (ledger, s, d) = row?;
        present = true;
        match ledger.as_str() {
            "discovery" => {
                scanned.0 = s;
                delivered.discovery = d;
            }
            "past_work" => {
                scanned.1 = s;
                delivered.past_work = d;
            }
            _ => {}
        }
    }
    Ok((present, delivered, scanned))
}

fn count_unsent_ledger(conn: &Connection, scanned: (u64, u64)) -> rusqlite::Result<u64> {
    if !table_exists(conn, "channel_ledger")? {
        return Ok(0);
    }
    let disc = i64::try_from(scanned.0).unwrap_or(i64::MAX);
    let past = i64::try_from(scanned.1).unwrap_or(i64::MAX);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM channel_ledger
         WHERE (ledger = 'discovery' AND seq > ?1)
            OR (ledger = 'past_work' AND seq > ?2)",
        params![disc, past],
        |row| row.get(0),
    )?;
    Ok(to_u64(count))
}

fn read_relay_cursor(
    conn: &Connection,
    root: &str,
) -> rusqlite::Result<Option<(Option<i64>, u64)>> {
    if !table_exists(conn, "relay_cursor")? {
        return Ok(None);
    }
    conn.query_row(
        "SELECT generation, after FROM relay_cursor WHERE endpoint = ?1",
        [root],
        |row| Ok((row.get(0)?, to_u64(row.get::<_, i64>(1)?))),
    )
    .optional()
}

fn count_unposted_relay(conn: &Connection, root: &str) -> rusqlite::Result<u64> {
    if !table_exists(conn, "relay_actions")? {
        return Ok(0);
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM relay_actions
         WHERE endpoint = ?1 AND (posted = 0 OR state = 'running')",
        [root],
        |row| row.get(0),
    )?;
    Ok(to_u64(count))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn now_millis() -> i64 {
    crate::post_land::submitter::now_millis()
}
