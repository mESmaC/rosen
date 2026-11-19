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

mod batch;
mod change_doc;
mod dirty;
mod facts;
mod log_lines;
mod machines_doc;
mod pass;
mod review_docs;
mod store;
mod unit;

use std::sync::Arc;
use std::time::Duration;

use rosen_protocol::EventKind;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::Instant;

pub use store::Cursor;

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

/// Why a pass failed. Every failure but [`PushError::Gone`] is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Reading the metadata or object store failed.
    Store(String),
    /// The endpoint's repository is archived or deleted (HTTP 410).
    Gone { endpoint: String },
    Send {
        endpoint: String,
        status: Option<u16>,
        message: String,
    },
}

/// What one step decided.
enum Step {
    /// Nothing to send before `until`, or before the next event.
    Wait { until: Option<Instant> },
    /// No endpoint, or a stopped one: only a land can change that.
    WaitForLand,
    /// A pass was sent; look again at once.
    Sent,
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
    pending: Pending,
    last_send: Option<Instant>,
    /// The machine list's fingerprint when last compared, and when.
    machines_seen: Option<String>,
    machines_checked: Option<Instant>,
    /// When the presence document last went.
    presence_sent: Option<Instant>,
}

/// Sends dirty documents until shutdown is requested.
pub async fn run(state: Arc<ServerState>) {
    let mut wake = state.events.watch();
    let mut session = Session::default();
    let mut failures: u32 = 0;
    loop {
        // Events after this point wake the next wait.
        let seen = *wake.borrow_and_update();
        let keep_going = match step(&state, &mut session).await {
            Ok(step) => {
                if failures > 0 && matches!(step, Step::Sent | Step::Wait { .. }) {
                    report(
                        &state,
                        json!({ "status": "recovered", "after_failures": failures }),
                    );
                    failures = 0;
                }
                match step {
                    Step::Sent => true,
                    Step::Wait { until } => wait_for_news(&state, &mut wake, until).await,
                    Step::WaitForLand => wait_for_land(&state, &mut wake, seen).await,
                }
            }
            Err(PushError::Gone { endpoint }) => {
                report_stopped(&state, &endpoint);
                session.stopped = Some(endpoint);
                session.target = None;
                failures = 0;
                wait_for_land(&state, &mut wake, seen).await
            }
            Err(error) => {
                session.target = None;
                failures = failures.saturating_add(1);
                let delay = retry_delay(failures);
                report_failure(&state, &error, failures, delay);
                sleep_unless_shutdown(&state, delay).await
            }
        };
        if !keep_going {
            return;
        }
    }
}

/// Reads new events and sends a pass when one is due.
async fn step(state: &Arc<ServerState>, session: &mut Session) -> Result<Step, PushError> {
    let Some(Feed { endpoint, channel }) = configured_feed(state, session).await? else {
        session.target = None;
        return Ok(Step::WaitForLand);
    };
    if session.stopped.as_deref() == Some(endpoint.as_str()) {
        return Ok(Step::WaitForLand);
    }
    session.stopped = None;
    if !matches!(&session.target, Some((target, _)) if target.endpoint == endpoint) {
        let (target, cursor) = reconcile(state, &endpoint).await?;
        session.pending = Pending::everything(cursor.scanned);
        session.target = Some((target, cursor));
    }
    scan(state, &mut session.pending, channel).await?;
    observe_machines(state, session).await?;
    let check = session
        .machines_checked
        .map_or_else(Instant::now, |at| at + MACHINE_CHECK);
    if session.pending.is_empty() {
        return Ok(Step::Wait { until: Some(check) });
    }
    let due = due(&session.pending, session.last_send);
    if Instant::now() < due {
        return Ok(Step::Wait {
            until: Some(due.min(check)),
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
    let delivered = pass::send(state, target, &session.pending, cursor, channel).await?;
    session.pending = Pending {
        scanned_to: session.pending.scanned_to,
        ..Pending::default()
    };
    if delivered.count > 0 {
        session.last_send = Some(Instant::now());
    }
    if delivered.presence {
        session.presence_sent = Some(Instant::now());
    }
    Ok(Step::Sent)
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

/// Looks up the credential, asks the endpoint what it holds, and takes the
/// stored cursor back to it if it lost documents. An endpoint never reached
/// starts at the log's tip: the full resend that follows carries all live
/// state, and closed changes are history the mirror already holds.
async fn reconcile(
    state: &Arc<ServerState>,
    endpoint: &str,
) -> Result<(Target, Cursor), PushError> {
    let credential = send::credential(&state.config.data_dir, endpoint).await;
    let target = Target {
        endpoint: endpoint.to_owned(),
        scratch: state.config.data_dir.join("state-push"),
        credential,
        stores: Stores::new(&state.config),
        relay: Arc::clone(&state.relay_wake),
    };
    let held = target.post(batch::probe()).await?;
    let (metadata, endpoint) = (state.config.metadata_path(), endpoint.to_owned());
    let tip = state.events.announced_seq();
    let cursor = blocking(move || {
        let store = StateStore::open(&metadata)?;
        let mut cursor = store.cursor(&endpoint)?.unwrap_or(Cursor {
            scanned: tip,
            delivered: held.min(tip),
        });
        cursor.reconcile(held);
        store.save_cursor(&endpoint, cursor)?;
        Ok(cursor)
    })
    .await?;
    Ok((target, cursor))
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

/// Waits for a new event or until `until`. False at shutdown.
async fn wait_for_news(
    state: &ServerState,
    wake: &mut watch::Receiver<u64>,
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
        () = deadline => true,
        () = state.shutdown_requested() => false,
    }
}

/// Waits for a land after `seen`, which may set or change the endpoint.
/// False at shutdown.
async fn wait_for_land(
    state: &Arc<ServerState>,
    wake: &mut watch::Receiver<u64>,
    seen: u64,
) -> bool {
    loop {
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
        if !wait_for_news(state, wake, None).await {
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

/// Publishes a `state_push` event for a failed pass (R9).
fn report_failure(state: &ServerState, error: &PushError, attempt: u32, retry_in: Duration) {
    let retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX);
    let (endpoint, status, message) = match error {
        PushError::Store(message) => (None, None, message.as_str()),
        PushError::Gone { endpoint } => (Some(endpoint.as_str()), Some(410), "gone"),
        PushError::Send {
            endpoint,
            status,
            message,
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

/// Publishes a `state_push` event when the endpoint answers 410, once,
/// since the feed then stops calling it.
fn report_stopped(state: &ServerState, endpoint: &str) {
    report(
        state,
        json!({
            "status": "stopped",
            "endpoint": endpoint,
            "http_status": 410,
            "error": "the endpoint's repository is archived or deleted; nothing more is sent \
                      until collapse changes or the server restarts",
        }),
    );
}

fn report(state: &ServerState, detail: serde_json::Value) {
    state.events.publish(EventKind::StatePush, None, detail);
}
