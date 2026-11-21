//! The state feed (R9): the server sends its live, unlanded state to the
//! endpoint under the base's `collapse` root, `<collapse>/rosen/state`, so
//! Collapse can show open changes, their gates, conflicts, threads,
//! approvals, claims and overlaps, review coverage of landed changes, the
//! configuration in force, post-land events, the machine list, presence
//! and, when the project opts in, channel lines (#406).
//!
//! It runs like the ledger push, with its sender, credential lookup and
//! backoff: one background task, off every request path, so nothing about
//! it blocks or fails a land, snapshot or review call. A cursor over the
//! event log, kept in the metadata store, says which documents are dirty;
//! nothing is sent on a timer, so an idle project wakes no endpoint.
//!
//! A pass sends whole documents, not deltas, each carrying the event
//! sequence it reflects, at most once every 10 s. One holding only
//! snapshots waits up to 60 s so a burst of them goes as one. On start and
//! after any failure the next pass first asks the endpoint what it holds,
//! then resends every live document, so the endpoint converges after any
//! loss.
//!
//! The machine list changes without an event (an approval, a credential
//! rotation, an agent subscribing), so while an endpoint is configured the
//! feed compares its fingerprint every 10 s. That is a local read: the
//! list goes only when it changed, after a minute with no further change
//! or with whatever goes sooner, and presence likewise.

pub(crate) mod batch;
pub(crate) mod change_doc;
mod dirty;
pub(crate) mod facts;
pub(crate) mod log_lines;
pub(crate) mod machines_doc;
pub(crate) mod pass;
pub(crate) mod platforms_doc;
pub(crate) mod review_docs;
mod store;
pub(crate) mod unit;

use std::sync::Arc;
use std::time::Duration;

use rosen_protocol::EventKind;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::Instant;

pub use store::Cursor;

use crate::collapse_feed::ResetWatch;
use crate::collapse_health::Feed as CollapseFeed;
use crate::ledger_push::{retry_delay, send, sleep_unless_shutdown};
use crate::ServerState;
use dirty::Pending;
use facts::Stores;
use pass::Target;
use store::StateStore;

/// No two sends start closer together than this.
const SEND_INTERVAL: Duration = Duration::from_secs(10);
/// How long a batch holding only snapshots waits for more.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(60);
/// How long other events wait for the ones a single action publishes with
/// them (a land's `landed`, `released` and `rebased`), so they go together.
const SETTLE: Duration = Duration::from_secs(2);
/// How long the machine list and presence wait for no further change
/// before going on their own.
const QUIET_WAIT: Duration = Duration::from_secs(60);
/// How often the machine list is compared with what was last seen.
const MACHINE_CHECK: Duration = Duration::from_secs(10);
/// While anyone is joined, presence rides on an outgoing batch at most
/// this often even unchanged, so Collapse knows how current its count is.
const PRESENCE_REFRESH: Duration = Duration::from_secs(5 * 60);

/// Why a pass failed. Every failure but [`PushError::Gone`] and
/// [`PushError::Diverged`] is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Reading the metadata or object store failed.
    Store(String),
    /// The endpoint's repository is archived or deleted (HTTP 410).
    Gone { endpoint: String },
    /// An unfollowed redirect (HTTP 301 or cross-origin 307/308) (#446).
    Moved {
        endpoint: String,
        status: u16,
        location: String,
    },
    /// Collapse holds feed state from another Rosen server (HTTP 409
    /// `source_changed`) or ahead of this server's local event log (#445).
    Diverged {
        endpoint: String,
        status: Option<u16>,
        message: String,
    },
    Send {
        endpoint: String,
        status: Option<u16>,
        message: String,
        no_credential: bool,
    },
}

const STOPPED_MESSAGE: &str = "the endpoint's repository is archived or deleted; nothing more is \
                               sent until collapse changes or the server restarts";

/// What one step decided.
enum Step {
    /// Nothing to send before `until`, or before the next event.
    Wait {
        until: Option<Instant>,
        endpoint: String,
        delivered: u64,
        empty: bool,
    },
    /// No endpoint, or a stopped one: only a land can change that.
    WaitForLand,
    /// A pass was sent; look again at once.
    Sent {
        endpoint: String,
        sent: usize,
        delivered: u64,
        scanned_to: u64,
    },
}

/// What the base's configuration asks the feed to do.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Feed {
    endpoint: String,
    /// Whether channel lines go too (`collapse_channel`).
    channel: bool,
}

/// What the task remembers between steps.
#[derive(Default)]
struct Session {
    /// The endpoint this run has reconciled with and its progress there.
    /// Cleared by a failure, so the next step asks the endpoint again what
    /// it holds and resends everything live.
    target: Option<(Target, Cursor)>,
    /// What the configuration named, and the event sequence it was read
    /// at; read again after any later land.
    configured: Option<(Option<Feed>, u64)>,
    /// The endpoint that answered 410.
    stopped: Option<String>,
    /// Followed same-origin 307/308 delivery URL for the session:
    /// `(configured_endpoint, delivery_endpoint)` (#446).
    redirected: Option<(String, String)>,
    /// The endpoint that reported `diverged` (409 `source_changed` or ahead of local log).
    diverged: Option<String>,
    pending: Pending,
    last_send: Option<Instant>,
    /// The machine list's fingerprint when last compared, and when.
    machines_seen: Option<String>,
    machines_checked: Option<Instant>,
    /// Last seen `ServerState::checkers` version (connects, disconnects, platform passes).
    checkers_seen: Option<u64>,
    /// When the presence document last went.
    presence_sent: Option<Instant>,
}

impl Session {
    fn clear_after_reset(&mut self) {
        self.target = None;
        self.stopped = None;
        self.diverged = None;
        self.pending = Pending::default();
        self.last_send = None;
        self.presence_sent = None;
    }
}

/// Sends dirty documents until shutdown is requested.
pub async fn run(state: Arc<ServerState>) {
    let mut wake = state.events.watch();
    let mut reset = ResetWatch::new(state.subscribe_collapse_reset());
    let mut session = Session::default();
    let mut failures: u32 = 0;
    loop {
        if reset.take() {
            session.clear_after_reset();
            failures = 0;
        }
        // Events after this point wake the next wait.
        let seen = *wake.borrow_and_update();
        let outcome = step(&state, &mut session).await;
        if !finish_step(
            &state,
            &mut session,
            &mut wake,
            &mut reset,
            &mut failures,
            seen,
            outcome,
        )
        .await
        {
            return;
        }
    }
}

async fn finish_step(
    state: &Arc<ServerState>,
    session: &mut Session,
    wake: &mut watch::Receiver<u64>,
    reset: &mut ResetWatch,
    failures: &mut u32,
    seen: u64,
    outcome: Result<Step, PushError>,
) -> bool {
    match outcome {
        Ok(Step::Sent {
            endpoint,
            sent,
            delivered,
            scanned_to,
        }) => {
            if *failures > 0 {
                report_recovered(state, &endpoint, sent, delivered, *failures);
                *failures = 0;
            }
            // Clears any prior transient failure without fabricating last_success_at
            // when a pass had nothing to deliver or every item was refused (#444).
            state
                .collapse
                .record_idle(CollapseFeed::StateFeed, &endpoint, Some(scanned_to));
            true
        }
        Ok(Step::Wait {
            until,
            endpoint,
            delivered,
            empty,
        }) => {
            if *failures > 0 && empty {
                report_recovered(state, &endpoint, 0, delivered, *failures);
                state
                    .collapse
                    .record_idle(CollapseFeed::StateFeed, &endpoint, None);
                *failures = 0;
            }
            wait_for_news(state, wake, reset, until).await
        }
        Ok(Step::WaitForLand) => {
            *failures = 0;
            wait_for_land(state, wake, reset, seen).await
        }
        Err(error) => finish_error(state, session, wake, reset, failures, seen, error).await,
    }
}

async fn finish_error(
    state: &Arc<ServerState>,
    session: &mut Session,
    wake: &mut watch::Receiver<u64>,
    reset: &mut ResetWatch,
    failures: &mut u32,
    seen: u64,
    error: PushError,
) -> bool {
    match error {
        PushError::Gone { endpoint } => {
            report_stopped(state, &endpoint);
            state
                .collapse
                .record_stopped(CollapseFeed::StateFeed, &endpoint, STOPPED_MESSAGE);
            session.stopped = Some(endpoint);
            session.target = None;
            *failures = 0;
            wait_for_land(state, wake, reset, seen).await
        }
        PushError::Diverged {
            endpoint,
            status,
            message,
        } => {
            report_diverged(state, &endpoint, status, &message);
            state
                .collapse
                .record_diverged(CollapseFeed::StateFeed, &endpoint, status, &message);
            session.diverged = Some(endpoint);
            session.target = None;
            *failures = 0;
            wait_for_land(state, wake, reset, seen).await
        }
        error => {
            session.target = None;
            *failures = failures.saturating_add(1);
            let delay = retry_delay(*failures);
            report_failure(state, &error, *failures, delay);
            record_failure(state, &error, delay);
            sleep_unless_shutdown(state, delay).await
        }
    }
}

/// Reads new events and sends a pass when one is due.
async fn step(state: &Arc<ServerState>, session: &mut Session) -> Result<Step, PushError> {
    // Only the mirror owner sends to Collapse (#433): a second server or
    // copied data directory counts its own event sequences and would collide.
    if !crate::mirror::owner::is_owner_async(state.config.data_dir.clone()).await {
        session.target = None;
        return Ok(Step::WaitForLand);
    }
    let Some(Feed { endpoint, channel }) = configured_feed(state, session).await? else {
        session.target = None;
        return Ok(Step::WaitForLand);
    };
    if session.stopped.as_deref() == Some(endpoint.as_str())
        || session.diverged.as_deref() == Some(endpoint.as_str())
    {
        return Ok(Step::WaitForLand);
    }
    session.stopped = None;
    session.diverged = None;
    if !matches!(&session.target, Some((target, _)) if target.endpoint == endpoint) {
        let _in_flight = state.collapse.begin(CollapseFeed::StateFeed, &endpoint);
        let (target, cursor) = reconcile(state, &endpoint, session).await?;
        session.pending = Pending::everything(cursor.scanned);
        session.target = Some((target, cursor));
    }
    scan(state, &mut session.pending, channel).await?;
    observe_machines(state, session).await?;
    observe_checkers(state, session);
    let check = session
        .machines_checked
        .map_or_else(Instant::now, |at| at + MACHINE_CHECK);
    let delivered = session
        .target
        .as_ref()
        .map_or(0, |(_, cursor)| cursor.delivered);
    if session.pending.is_empty() {
        state.collapse.set_pending(
            CollapseFeed::StateFeed,
            &endpoint,
            false,
            Some(session.pending.scanned_to),
        );
        return Ok(Step::Wait {
            until: Some(check),
            endpoint,
            delivered,
            empty: true,
        });
    }
    let due = due(&session.pending, session.last_send);
    if Instant::now() < due {
        state
            .collapse
            .set_pending(CollapseFeed::StateFeed, &endpoint, true, None);
        return Ok(Step::Wait {
            until: Some(due.min(check)),
            endpoint,
            delivered,
            empty: false,
        });
    }
    send_pass(state, session, channel).await
}

/// Sends the pending documents and lines, with presence if a refresh of
/// it is due.
async fn send_pass(
    state: &Arc<ServerState>,
    session: &mut Session,
    channel: bool,
) -> Result<Step, PushError> {
    session.pending.refresh_presence = session
        .presence_sent
        .is_none_or(|at| at.elapsed() >= PRESENCE_REFRESH);
    let (target, cursor) = session.target.as_mut().expect("reconciled above");
    let endpoint = target.endpoint.clone();
    state
        .collapse
        .set_pending(CollapseFeed::StateFeed, &endpoint, true, None);
    let _in_flight = state.collapse.begin(CollapseFeed::StateFeed, &endpoint);
    let delivered = pass::send(state, target, &session.pending, cursor, channel).await?;
    let delivery_endpoint = target.delivery_endpoint.clone();
    let scanned_to = session.pending.scanned_to;
    let cursor_delivered = cursor.delivered;
    if delivery_endpoint != endpoint {
        session.redirected = Some((endpoint.clone(), delivery_endpoint));
    }
    session.pending = Pending {
        scanned_to,
        ..Pending::default()
    };
    if delivered.any_accepted || delivered.count > 0 {
        session.last_send = Some(Instant::now());
    }
    if delivered.presence {
        session.presence_sent = Some(Instant::now());
    }
    Ok(Step::Sent {
        endpoint,
        sent: delivered.count,
        delivered: cursor_delivered,
        scanned_to,
    })
}

/// Compares the machine list with what was last seen, at most every
/// [`MACHINE_CHECK`], and marks it dirty when it changed.
async fn observe_machines(
    state: &Arc<ServerState>,
    session: &mut Session,
) -> Result<(), PushError> {
    if session
        .machines_checked
        .is_some_and(|at| at.elapsed() < MACHINE_CHECK)
    {
        return Ok(());
    }
    session.machines_checked = Some(Instant::now());
    let metadata = state.config.metadata_path();
    let running = pass::running_agents(state);
    let now = crate::machines::now();
    let unit = blocking(move || machines_doc::machines(&metadata, &running, now)).await?;
    let seen = session.machines_seen.replace(unit.fingerprint.clone());
    // The first look only records: the resend after reconciling sends it.
    if seen.is_some_and(|seen| seen != unit.fingerprint) {
        session.pending.machines_changed();
    }
    Ok(())
}

/// Notices checker connects, disconnects and per-platform checkpoint passes.
fn observe_checkers(state: &ServerState, session: &mut Session) {
    let version = state.checkers.version();
    if session
        .checkers_seen
        .replace(version)
        .is_some_and(|seen| seen != version)
    {
        session.pending.platforms_changed();
    }
}

/// When the pending documents should go: soon after the first event other
/// than a snapshot, a minute after the first snapshot or after the machine
/// list or presence stopped changing, whichever is soonest, and never
/// sooner than [`SEND_INTERVAL`] after the last send.
fn due(pending: &Pending, last_send: Option<Instant>) -> Instant {
    let now = Instant::now();
    let wanted = [
        pending.urgent_since.map(|since| since + SETTLE),
        pending.snapshot_since.map(|since| since + SNAPSHOT_WAIT),
        pending.quiet_since.map(|since| since + QUIET_WAIT),
    ]
    .into_iter()
    .flatten()
    .min()
    .map_or(now, Instant::from_std);
    match last_send {
        Some(last) => wanted.max(last + SEND_INTERVAL),
        None => wanted,
    }
}

/// What the base's configuration asks for, read again only after a land,
/// which is the only thing that can change it.
async fn configured_feed(
    state: &Arc<ServerState>,
    session: &mut Session,
) -> Result<Option<Feed>, PushError> {
    if let Some((feed, read_at)) = &session.configured {
        let read_at = *read_at;
        let reading = Arc::clone(state);
        let landed = tokio::task::spawn_blocking(move || {
            reading.events.holds_kind_after(read_at, EventKind::Landed)
        })
        .await
        .map_err(|join| PushError::Store(join.to_string()))?
        .map_err(|error| PushError::Store(error.to_string()))?;
        if !landed {
            return Ok(feed.clone());
        }
    }
    let read_at = state.events.announced_seq();
    let in_force = crate::ledger_push::in_force(&state.config)
        .await
        .map_err(PushError::Store)?;
    let feed = in_force.and_then(|config| {
        let endpoint = config.state_feed_url()?;
        let channel = config.collapse_channel;
        Some(Feed { endpoint, channel })
    });
    session.configured = Some((feed.clone(), read_at));
    Ok(feed)
}

/// Looks up the credential, asks the endpoint what it holds, checks for
/// divergence against the local event log (#445), and takes the stored cursor
/// back to it if it lost documents. An endpoint never reached starts at the
/// log's tip: the full resend that follows carries all live state, and closed
/// changes are history the mirror already holds.
async fn reconcile(
    state: &Arc<ServerState>,
    endpoint: &str,
    session: &mut Session,
) -> Result<(Target, Cursor), PushError> {
    let metadata = state.config.metadata_path();
    let feed_id = blocking(move || crate::collapse_feed::get_or_create(&metadata)).await?;
    let credential = send::credential(&state.config.data_dir, endpoint).await;
    let delivery_endpoint = match &session.redirected {
        Some((configured, target)) if configured == endpoint => target.clone(),
        _ => {
            session.redirected = None;
            endpoint.to_owned()
        }
    };
    let mut target = Target {
        endpoint: endpoint.to_owned(),
        delivery_endpoint,
        feed_id,
        scratch: state.config.data_dir.join("state-push"),
        credential,
        stores: Stores::new(&state.config),
        relay: Arc::clone(&state.relay_wake),
    };
    let probe = batch::probe(Some(&target.feed_id));
    let held = target.post(state, probe).await?;
    if target.delivery_endpoint != endpoint {
        session.redirected = Some((endpoint.to_owned(), target.delivery_endpoint.clone()));
    }
    // A probe delivers no items, so it never advances last_success_at (#444).
    let (metadata, endpoint_owned) = (state.config.metadata_path(), endpoint.to_owned());
    let announced = state.events.announced_seq();
    let reconciled = blocking(move || {
        let store = StateStore::open(&metadata)?;
        let local_latest = crate::collapse_feed::local_event_seq(store.connection())
            .unwrap_or(0)
            .max(announced);
        if held > local_latest {
            return Ok(Err(local_latest));
        }
        let mut cursor = store.cursor(&endpoint_owned)?.unwrap_or(Cursor {
            scanned: local_latest,
            delivered: held.min(local_latest),
        });
        cursor.reconcile(held);
        store.save_cursor(&endpoint_owned, cursor)?;
        Ok(Ok(cursor))
    })
    .await?;
    match reconciled {
        Ok(cursor) => Ok((target, cursor)),
        Err(local_latest) => Err(PushError::Diverged {
            endpoint: endpoint.to_owned(),
            status: None,
            message: format!(
                "Collapse holds state through event {held}, ahead of this server's latest event \
                 {local_latest}; run `rosen collapse link --reset` to replace Collapse's feed state"
            ),
        }),
    }
}

async fn scan(
    state: &Arc<ServerState>,
    pending: &mut Pending,
    channel: bool,
) -> Result<(), PushError> {
    let reading = Arc::clone(state);
    let mut scanned = std::mem::take(pending);
    let scanned = tokio::task::spawn_blocking(move || {
        scanned.scan(&reading.events, channel).map(|()| scanned)
    })
    .await
    .map_err(|join| PushError::Store(join.to_string()))?
    .map_err(|error| PushError::Store(error.to_string()))?;
    *pending = scanned;
    Ok(())
}

/// Waits for a new event, a Collapse feed reset, or until `until`. False at shutdown.
async fn wait_for_news(
    state: &ServerState,
    wake: &mut watch::Receiver<u64>,
    reset: &mut ResetWatch,
    until: Option<Instant>,
) -> bool {
    let deadline = async {
        match until {
            Some(until) => tokio::time::sleep_until(until).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        changed = wake.changed() => changed.is_ok(),
        ok = reset.changed() => ok,
        () = deadline => true,
        () = state.shutdown_requested() => false,
    }
}

/// Waits for a land after `seen`, or a Collapse feed reset, which may set,
/// change or reset the endpoint. False at shutdown.
async fn wait_for_land(
    state: &Arc<ServerState>,
    wake: &mut watch::Receiver<u64>,
    reset: &mut ResetWatch,
    seen: u64,
) -> bool {
    loop {
        if reset.pending() {
            return true;
        }
        let reading = Arc::clone(state);
        let landed = tokio::task::spawn_blocking(move || {
            reading.events.holds_kind_after(seen, EventKind::Landed)
        })
        .await;
        match landed {
            Ok(Ok(true)) => return true,
            Ok(Ok(false)) => {}
            Ok(Err(error)) => eprintln!("rosen-server: state feed could not read events: {error}"),
            Err(join) => eprintln!("rosen-server: state feed could not read events: {join}"),
        }
        if !wait_for_news(state, wake, reset, None).await {
            return false;
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, PushError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|join| PushError::Store(join.to_string()))?
        .map_err(|error| PushError::Store(error.to_string()))
}

fn record_failure(state: &ServerState, error: &PushError, retry_in: Duration) {
    if let PushError::Moved {
        endpoint,
        status,
        location,
    } = error
    {
        let message = format!("HTTP {status}: moved to {location} (redirect not followed)");
        state.collapse.record_moved_unfollowed(
            CollapseFeed::StateFeed,
            endpoint,
            *status,
            location,
            &message,
            retry_in,
        );
        return;
    }
    let (endpoint, status, message, no_credential) = match error {
        PushError::Store(message) => (None, None, message.as_str(), false),
        PushError::Gone { endpoint } => (Some(endpoint.as_str()), Some(410), "gone", false),
        PushError::Moved { .. } => unreachable!(),
        PushError::Diverged {
            endpoint,
            status,
            message,
        } => (Some(endpoint.as_str()), *status, message.as_str(), false),
        PushError::Send {
            endpoint,
            status,
            message,
            no_credential,
        } => (
            Some(endpoint.as_str()),
            *status,
            message.as_str(),
            *no_credential,
        ),
    };
    state.collapse.record_failure(
        CollapseFeed::StateFeed,
        endpoint,
        status,
        message,
        retry_in,
        no_credential,
    );
}

/// Publishes a `state_push` event for a failed pass (R9).
fn report_failure(state: &ServerState, error: &PushError, attempt: u32, retry_in: Duration) {
    let retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX);
    if let PushError::Moved {
        endpoint,
        status,
        location,
    } = error
    {
        report(
            state,
            json!({
                "status": "moved",
                "endpoint": endpoint,
                "http_status": status,
                "location": location,
                "hint": send::moved_hint(location),
                "attempt": attempt,
                "retry_in_ms": retry_in_ms,
                "error": format!("HTTP {status}: moved to {location} (redirect not followed)"),
            }),
        );
        return;
    }
    let (endpoint, status, message) = match error {
        PushError::Store(message) => (None, None, message.as_str()),
        PushError::Gone { endpoint } => (Some(endpoint.as_str()), Some(410), "gone"),
        PushError::Moved { .. } => unreachable!(),
        PushError::Diverged {
            endpoint,
            status,
            message,
        } => (Some(endpoint.as_str()), *status, message.as_str()),
        PushError::Send {
            endpoint,
            status,
            message,
            ..
        } => (Some(endpoint.as_str()), *status, message.as_str()),
    };
    report(
        state,
        json!({
            "status": "failed",
            "endpoint": endpoint,
            "http_status": status,
            "attempt": attempt,
            "retry_in_ms": retry_in_ms,
            "error": message,
        }),
    );
}

/// Publishes a `state_push` event when a same-origin 307/308 redirect is
/// followed to `location` (#446).
pub(crate) fn report_moved(state: &ServerState, endpoint: &str, http_status: u16, location: &str) {
    report(
        state,
        json!({
            "status": "moved",
            "endpoint": endpoint,
            "http_status": http_status,
            "location": location,
            "hint": send::moved_hint(location),
        }),
    );
}

/// Publishes a `state_push` event when the endpoint answers 410, once,
/// since the feed then stops calling it.
fn report_stopped(state: &ServerState, endpoint: &str) {
    report(
        state,
        json!({
            "status": "stopped",
            "endpoint": endpoint,
            "http_status": 410,
            "error": STOPPED_MESSAGE,
        }),
    );
}

/// Publishes a `state_push` event when the feed detects divergence (HTTP 409
/// `source_changed` or Collapse ahead of the local event log), once, since
/// the feed then pauses until `rosen collapse link --reset` (#445).
fn report_diverged(state: &ServerState, endpoint: &str, status: Option<u16>, message: &str) {
    report(
        state,
        json!({
            "status": "diverged",
            "endpoint": endpoint,
            "http_status": status,
            "error": message,
        }),
    );
}

/// Publishes a `state_push` event when a pass succeeds after failures,
/// matching `ledger_push`'s `sent`-after-failures event (R9, #434).
fn report_recovered(
    state: &ServerState,
    endpoint: &str,
    sent: usize,
    delivered: u64,
    failures: u32,
) {
    report(
        state,
        json!({
            "status": "sent",
            "endpoint": endpoint,
            "sent": sent,
            "delivered": delivered,
            "after_failures": failures,
        }),
    );
}

/// Publishes a `state_push` event when a single document/item is refused by
/// the endpoint (HTTP 400, 413 or 422) and skipped so the rest of the batch
/// can be delivered (R9, #444).
pub(crate) fn report_refused(
    state: &ServerState,
    endpoint: &str,
    document: &str,
    section: unit::Section,
    http_status: u16,
    error: &str,
) {
    report(
        state,
        json!({
            "status": "refused",
            "endpoint": endpoint,
            "document": document,
            "section": section.key(),
            "http_status": http_status,
            "error": error,
        }),
    );
}

fn report(state: &ServerState, detail: serde_json::Value) {
    state.events.publish(EventKind::StatePush, None, detail);
}
