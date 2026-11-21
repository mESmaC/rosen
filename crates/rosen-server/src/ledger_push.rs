//! The ledger push (SPEC R13): after a Discoveries or Past work entry is
//! appended, the server sends it to the endpoint the base's `rosen.toml`
//! names in `ledger_endpoint`, or else under its `collapse` root, such as
//! Collapse's read-only copy of the ledgers.
//!
//! It runs like the mirror push: one background task per server, off the
//! append path. An append only commits its row and channel line, which
//! wakes the task, and the task sends whatever the ledgers hold by then, so
//! appends during a send ride the next one. A failed send is retried with
//! backoff and publishes a `ledger_push` event; nothing about it blocks or
//! fails an append.
//!
//! The endpoint stores entries by sequence, so sending one twice changes
//! nothing, and answers with the highest sequence it holds in each ledger.
//! On start and after each failure the task first asks it with an empty
//! batch, and resends from there when the endpoint holds less than the
//! server last delivered.

mod cursor;
pub(crate) mod entries;
pub(crate) mod send;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rosen_core::ProjectConfig;
use rosen_protocol::EventKind;
use rosen_store::LooseObjectStore;
use serde_json::json;
use tokio::sync::watch;

pub use cursor::{Cursor, Cursors, Held, PerLedger};
pub use entries::{rfc3339_utc, MAX_BODY, MAX_ENTRIES};
pub use send::SendFailure;

use crate::collapse_feed::ResetWatch;
use crate::collapse_health::{Feed, Tracker};
use crate::events::EventLog;
use crate::landing::Ledger;
use crate::project_config;
use crate::{ServerConfig, ServerState};
use cursor::{CursorStore, Refusal};
use entries::BatchEntry;

/// The first retry waits this long; each further failure doubles the wait.
const RETRY_START: Duration = Duration::from_secs(1);
/// The longest wait between retries, so an endpoint that comes back is
/// caught up within minutes without being called many times a second.
const RETRY_CAP: Duration = Duration::from_secs(5 * 60);

/// What one pass found and did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// The base's configuration names no ledger endpoint.
    NotConfigured,
    /// This server is not the mirror owner, so nothing is sent (#433).
    NotOwner,
    /// The endpoint answered 410 earlier; nothing is sent until it changes.
    Stopped { endpoint: String },
    /// Collapse holds feed state from another Rosen server or ahead of local
    /// ledgers; nothing is sent until `rosen collapse link --reset` (#445).
    Diverged { endpoint: String },
    /// The endpoint holds every entry; `sent` entries went in this pass.
    Current {
        endpoint: String,
        sent: usize,
        held: Held,
    },
}

/// Why a pass failed. Every failure but [`PushError::Gone`] and
/// [`PushError::Diverged`] is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Reading or writing the metadata store failed.
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
    /// `source_changed`) or ahead of this server's local ledgers (#445).
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
                               sent until ledger_endpoint changes or the server restarts";

/// What the task remembers between passes.
#[derive(Debug, Default)]
pub struct Session {
    /// The endpoint whose holdings this run has read, if any. Cleared by a
    /// failure, so the next pass asks again what the endpoint holds and
    /// looks its credential up again.
    reconciled: Option<Reconciled>,
    /// The endpoint that answered 410.
    stopped: Option<String>,
    /// Followed same-origin 307/308 delivery URL for the session:
    /// `(configured_endpoint, delivery_endpoint)` (#446).
    redirected: Option<(String, String)>,
    /// The endpoint that reported `diverged` (409 `source_changed` or ahead of local ledgers).
    diverged: Option<String>,
}

/// An endpoint this run has reached, with the credential and feed ID found
/// for it. The credential is kept because a lookup can take seconds: Git
/// Credential Manager probes an unfamiliar host before it answers.
#[derive(Debug)]
struct Reconciled {
    endpoint: String,
    feed_id: String,
    credential: Option<send::Credential>,
}

/// Sends new ledger entries until shutdown is requested.
pub async fn run(state: Arc<ServerState>) {
    let mut wake = state.events.watch();
    let mut reset = ResetWatch::new(state.subscribe_collapse_reset());
    let mut session = Session::default();
    let mut failures: u32 = 0;
    loop {
        if reset.take() {
            session.reconciled = None;
            session.stopped = None;
            session.diverged = None;
            failures = 0;
        }
        // Appends after this point wake the next pass, even if they arrive
        // while this one is sending.
        let seen = state.events.announced_seq();
        let outcome = push_once(&state, &mut session).await;
        if !finish_pass(
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

async fn finish_pass(
    state: &Arc<ServerState>,
    session: &mut Session,
    wake: &mut watch::Receiver<u64>,
    reset: &mut ResetWatch,
    failures: &mut u32,
    seen: u64,
    outcome: Result<Pushed, PushError>,
) -> bool {
    match outcome {
        Ok(pushed) => {
            if *failures > 0 {
                report_recovered(state, &pushed, *failures);
            }
            if let Pushed::Current { endpoint, .. } = &pushed {
                state.collapse.record_idle(Feed::LedgerPush, endpoint, None);
            }
            *failures = 0;
            // Without an endpoint only a land or reset can change the outcome.
            let sending = matches!(pushed, Pushed::Current { .. });
            wait_for_news(state, wake, reset, seen, sending).await
        }
        Err(PushError::Gone { endpoint }) => {
            report_stopped(state, &endpoint);
            state
                .collapse
                .record_stopped(Feed::LedgerPush, &endpoint, STOPPED_MESSAGE);
            session.stopped = Some(endpoint);
            session.reconciled = None;
            *failures = 0;
            wait_for_news(state, wake, reset, seen, false).await
        }
        Err(PushError::Diverged {
            endpoint,
            status,
            message,
        }) => {
            report_diverged(state, &endpoint, status, &message);
            state
                .collapse
                .record_diverged(Feed::LedgerPush, &endpoint, status, &message);
            session.diverged = Some(endpoint);
            session.reconciled = None;
            *failures = 0;
            wait_for_news(state, wake, reset, seen, false).await
        }
        Err(error) => {
            session.reconciled = None;
            *failures = failures.saturating_add(1);
            let delay = retry_delay(*failures);
            report_failure(state, &error, *failures, delay);
            record_failure(state, &error, delay);
            sleep_unless_shutdown(state, delay).await
        }
    }
}

/// Brings the endpoint up to the ledgers once.
pub async fn push_once(state: &ServerState, session: &mut Session) -> Result<Pushed, PushError> {
    if !crate::mirror::owner::is_owner_async(state.config.data_dir.clone()).await {
        session.reconciled = None;
        return Ok(Pushed::NotOwner);
    }
    let Some(endpoint) = configured_endpoint(&state.config).await? else {
        return Ok(Pushed::NotConfigured);
    };
    if session.stopped.as_deref() == Some(endpoint.as_str()) {
        return Ok(Pushed::Stopped { endpoint });
    }
    if session.diverged.as_deref() == Some(endpoint.as_str()) {
        return Ok(Pushed::Diverged { endpoint });
    }
    session.stopped = None;
    session.diverged = None;
    push_to_with(
        &state.config,
        &endpoint,
        session,
        Some(&state.collapse),
        Some(&state.events),
    )
    .await
}

/// Sends every entry `endpoint` lacks, one batch at a time. Public so a
/// test can aim it at a local endpoint without a project configuration.
pub async fn push_to(
    config: &ServerConfig,
    endpoint: &str,
    session: &mut Session,
) -> Result<Pushed, PushError> {
    push_to_with(config, endpoint, session, None, None).await
}

async fn push_to_with(
    config: &ServerConfig,
    endpoint: &str,
    session: &mut Session,
    tracker: Option<&Tracker>,
    events: Option<&EventLog>,
) -> Result<Pushed, PushError> {
    // Only the mirror owner sends ledger entries (#433): another server's
    // sequence numbers would collide and hide the owner's entries.
    if !crate::mirror::owner::is_owner_async(config.data_dir.clone()).await {
        session.reconciled = None;
        return Ok(Pushed::NotOwner);
    }
    let _in_flight = tracker.map(|t| t.begin(Feed::LedgerPush, endpoint));
    let (mut target, mut held) = prepare_target(config, endpoint, session, tracker, events).await?;
    let mut sent = 0;
    loop {
        let batch = target.next_batch().await?;
        if batch.is_empty() {
            return Ok(Pushed::Current {
                endpoint: endpoint.to_owned(),
                sent,
                held,
            });
        }
        let extent = batch.extent;
        if !batch.entries.is_empty() {
            sent += send_batch(
                &mut target,
                batch.entries,
                &mut held,
                session,
                tracker,
                events,
            )
            .await?;
        }
        // Advance scanned past any skipped rows covered by this batch.
        target.update(move |cursors| cursors.skip(&extent)).await?;
    }
}

/// Sends `entries` as one batch, splitting by halves on HTTP 400, 413 or 422
/// to isolate and skip refused entries while delivering the rest (#444).
async fn send_batch(
    target: &mut Target,
    entries: Vec<BatchEntry>,
    held: &mut Held,
    session: &mut Session,
    tracker: Option<&Tracker>,
    events: Option<&EventLog>,
) -> Result<usize, PushError> {
    let mut sent = 0;
    let mut stack = vec![entries];
    while let Some(chunk) = stack.pop() {
        let body = entries::encode(Some(&target.feed_id), &chunk);
        match target.send(body).await {
            Ok((answer, redirected)) => {
                if let Some((status, new_url)) = redirected {
                    note_redirect(session, tracker, events, &target.endpoint, status, &new_url);
                }
                *held = answer;
                let sub = entries::extent_of(&chunk);
                let kept = target
                    .update(move |cursors| cursors.acknowledge(&sub, &answer))
                    .await?;
                if !kept {
                    return Err(target.failure(None, lost_batch(&sub, &answer)));
                }
                if let Some(t) = tracker {
                    t.record_success(Feed::LedgerPush, &target.endpoint);
                }
                sent += chunk.len();
            }
            Err(PushError::Send { status, .. })
                if send::is_batch_refusal(status) && chunk.len() > 1 =>
            {
                let mut left = chunk;
                let right = left.split_off(left.len() / 2);
                stack.push(right);
                stack.push(left);
            }
            Err(PushError::Send {
                status: Some(status),
                message,
                ..
            }) if send::is_batch_refusal(Some(status)) => {
                record_refused(target, events, &chunk[0], status, &message).await?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(sent)
}

async fn record_refused(
    target: &Target,
    events: Option<&EventLog>,
    entry: &BatchEntry,
    status: u16,
    message: &str,
) -> Result<(), PushError> {
    let document = format!("{}:{}", entry.ledger.as_str(), entry.seq);
    let refusal = Refusal {
        document: document.clone(),
        ledger: entry.ledger,
        seq: entry.seq,
        http_status: status,
        message: message.to_owned(),
        at: crate::post_land::submitter::now_millis(),
    };
    let (metadata, endpoint) = (target.metadata.clone(), target.endpoint.clone());
    blocking(move || {
        let mut store = CursorStore::open(&metadata)?;
        store.record_refused(&endpoint, &refusal)
    })
    .await?;
    if let Some(events) = events {
        report_refused(
            events,
            &target.endpoint,
            &document,
            entry.ledger.as_str(),
            entry.seq,
            status,
            message,
        );
    }
    Ok(())
}

async fn prepare_target(
    config: &ServerConfig,
    endpoint: &str,
    session: &mut Session,
    tracker: Option<&Tracker>,
    events: Option<&EventLog>,
) -> Result<(Target, Held), PushError> {
    let fresh = !matches!(&session.reconciled, Some(known) if known.endpoint == endpoint);
    let (credential, feed_id) = match &session.reconciled {
        Some(known) if !fresh => (known.credential.clone(), known.feed_id.clone()),
        _ => {
            let metadata = config.metadata_path();
            let feed_id = blocking(move || crate::collapse_feed::get_or_create(&metadata)).await?;
            let credential = send::credential(&config.data_dir, endpoint).await;
            (credential, feed_id)
        }
    };
    let delivery = match &session.redirected {
        Some((configured, target)) if configured == endpoint => target.as_str(),
        _ => {
            session.redirected = None;
            endpoint
        }
    };
    let mut target = Target::new(config, endpoint, delivery, feed_id, credential);
    let mut held = Held::default();
    if fresh {
        let (h, redirected) = target
            .send(entries::empty_body(Some(&target.feed_id)))
            .await?;
        if let Some((status, new_url)) = redirected {
            note_redirect(session, tracker, events, endpoint, status, &new_url);
        }
        held = h;
        // A probe delivers no entries, so it never advances last_success_at (#444).
        let local_latest = target.local_latest().await?;
        if held.discovery > local_latest.discovery || held.past_work > local_latest.past_work {
            return Err(PushError::Diverged {
                endpoint: endpoint.to_owned(),
                status: None,
                message: format!(
                    "Collapse holds ledger entries through discovery {} and past_work {}, ahead \
                     of this server's latest {} and {}; run `rosen collapse link --reset` to \
                     replace Collapse's feed state",
                    held.discovery, held.past_work, local_latest.discovery, local_latest.past_work
                ),
            });
        }
        target
            .update(move |cursors| {
                cursors.reconcile(&held);
                true
            })
            .await?;
        session.reconciled = Some(Reconciled {
            endpoint: endpoint.to_owned(),
            feed_id: target.feed_id.clone(),
            credential: target.credential.clone(),
        });
    }
    Ok((target, held))
}

fn note_redirect(
    session: &mut Session,
    tracker: Option<&Tracker>,
    events: Option<&EventLog>,
    endpoint: &str,
    status: u16,
    new_url: &str,
) {
    let changed = session
        .redirected
        .as_ref()
        .is_none_or(|(cfg, dst)| cfg != endpoint || dst != new_url);
    session.redirected = Some((endpoint.to_owned(), new_url.to_owned()));
    if let Some(t) = tracker {
        t.record_moved(Feed::LedgerPush, endpoint, new_url);
    }
    if changed {
        if let Some(events) = events {
            report_moved(events, endpoint, status, new_url);
        }
    }
}

/// One endpoint and what reaching it needs.
struct Target {
    /// Configured endpoint from `rosen.toml` (cursor and health tracker key).
    endpoint: String,
    /// Active delivery URL for the session (follows same-origin 307/308).
    delivery_endpoint: String,
    feed_id: String,
    metadata: PathBuf,
    scratch: PathBuf,
    credential: Option<send::Credential>,
}

impl Target {
    fn new(
        config: &ServerConfig,
        endpoint: &str,
        delivery_endpoint: &str,
        feed_id: String,
        credential: Option<send::Credential>,
    ) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            delivery_endpoint: delivery_endpoint.to_owned(),
            feed_id,
            metadata: config.metadata_path(),
            scratch: config.data_dir.join("ledger-push"),
            credential,
        }
    }

    async fn send(&mut self, body: Vec<u8>) -> Result<(Held, Option<(u16, String)>), PushError> {
        let credential = self.credential.as_ref();
        match send::post_following(&self.scratch, &self.delivery_endpoint, &body, credential).await
        {
            Ok((held, redirected)) => {
                if let Some((_, ref new_url)) = redirected {
                    self.delivery_endpoint = new_url.clone();
                }
                Ok((held, redirected))
            }
            Err(SendFailure::Gone) => Err(PushError::Gone {
                endpoint: self.endpoint.clone(),
            }),
            Err(SendFailure::Moved { status, location }) => Err(PushError::Moved {
                endpoint: self.endpoint.clone(),
                status,
                location,
            }),
            Err(SendFailure::SourceChanged { message }) => Err(PushError::Diverged {
                endpoint: self.endpoint.clone(),
                status: Some(409),
                message,
            }),
            Err(SendFailure::Failed { status, message }) => Err(self.failure(status, message)),
        }
    }

    async fn local_latest(&self) -> Result<Held, PushError> {
        let metadata = self.metadata.clone();
        blocking(move || {
            let store = CursorStore::open(&metadata)?;
            crate::collapse_feed::local_ledger_seq(store.connection())
                .map_err(rusqlite::Error::InvalidParameterName)
        })
        .await
    }

    fn failure(&self, status: Option<u16>, message: String) -> PushError {
        let no_credential = self.endpoint.starts_with("https://")
            && self.credential.is_none()
            && matches!(status, Some(401 | 403));
        let message = match (&self.credential, status) {
            (None, Some(401 | 403)) => format!(
                "{message} (git's credential helper holds no token for {})",
                self.endpoint
            ),
            _ => message,
        };
        PushError::Send {
            endpoint: self.endpoint.clone(),
            status,
            message,
            no_credential,
        }
    }

    /// The next batch after the stored cursors, read on the blocking pool.
    async fn next_batch(&self) -> Result<entries::Batch, PushError> {
        let (metadata, endpoint, feed_id) = (
            self.metadata.clone(),
            self.endpoint.clone(),
            self.feed_id.clone(),
        );
        blocking(move || {
            let store = CursorStore::open(&metadata)?;
            let cursors = store.load(&endpoint)?;
            let refused = store.refused_keys(&endpoint)?;
            entries::next_batch(store.connection(), &cursors, &refused, Some(&feed_id))
        })
        .await
    }

    /// Applies `change` to the stored cursors and saves them, returning
    /// what `change` returned.
    async fn update(
        &self,
        change: impl FnOnce(&mut Cursors) -> bool + Send + 'static,
    ) -> Result<bool, PushError> {
        let (metadata, endpoint) = (self.metadata.clone(), self.endpoint.clone());
        blocking(move || {
            let mut store = CursorStore::open(&metadata)?;
            let mut cursors = store.load(&endpoint)?;
            let result = change(&mut cursors);
            store.save(&endpoint, &cursors)?;
            Ok(result)
        })
        .await
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

fn lost_batch(extent: &cursor::Extent, held: &Held) -> String {
    format!(
        "the endpoint accepted a batch through discovery {} and past_work {} but reports holding \
         only {} and {}; resending from there",
        extent.discovery.delivered, extent.past_work.delivered, held.discovery, held.past_work
    )
}

/// The endpoint the base's configuration names, if any. Read on every
/// pass, so a land that sets or changes it takes effect at once.
async fn configured_endpoint(config: &ServerConfig) -> Result<Option<String>, PushError> {
    let in_force = in_force(config).await.map_err(PushError::Store)?;
    Ok(in_force.and_then(|config| config.ledger_push_url()))
}

/// The configuration in force at the base, or `None` before an import.
/// Shared with the state feed, which reads its endpoint the same way.
pub(crate) async fn in_force(config: &ServerConfig) -> Result<Option<Arc<ProjectConfig>>, String> {
    let (metadata, objects) = (config.metadata_path(), config.objects_dir());
    let found = tokio::task::spawn_blocking(move || -> Result<_, String> {
        // A server that has never imported has no database; opening one
        // here would create it.
        if !metadata.is_file() {
            return Ok(None);
        }
        let base = Ledger::open(&metadata)
            .and_then(|ledger| ledger.project_base())
            .map_err(|e| e.to_string())?;
        let Some((_, commit)) = base else {
            return Ok(None);
        };
        let store = LooseObjectStore::open(objects).map_err(|e| e.to_string())?;
        let in_force = project_config::resolve(&store, &commit).map_err(|e| e.to_string())?;
        Ok(Some(in_force.config))
    })
    .await
    .map_err(|join| join.to_string())?;
    found
}

/// Waits until an event after `seen` or a Collapse feed reset can change what
/// a pass would do: a land, which can change the endpoint, a reset, or, while
/// `sending`, a channel line, which an append writes. Returns false when the
/// server is shutting down.
async fn wait_for_news(
    state: &Arc<ServerState>,
    wake: &mut watch::Receiver<u64>,
    reset: &mut ResetWatch,
    seen: u64,
    sending: bool,
) -> bool {
    let mut seen = seen;
    loop {
        if reset.pending() {
            return true;
        }
        // Marked before the read: an event announced after this wakes the
        // next pass, and every event up to `tip` is visible to the read.
        let tip = *wake.borrow_and_update();
        match news_after(state, seen, sending).await {
            Ok(true) => return true,
            Ok(false) => seen = seen.max(tip),
            // Keeps `seen`, so the next wake reads these events again.
            Err(err) => eprintln!("rosen-server: ledger push could not read events: {err}"),
        }
        tokio::select! {
            changed = wake.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            ok = reset.changed() => {
                return ok;
            }
            () = state.shutdown_requested() => return false,
        }
    }
}

async fn news_after(state: &Arc<ServerState>, seen: u64, sending: bool) -> Result<bool, String> {
    let state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        let events = &state.events;
        Ok(events.holds_kind_after(seen, EventKind::Landed)?
            || (sending && events.holds_kind_after(seen, EventKind::Channel)?))
    })
    .await
    .map_err(|join| join.to_string())?
    .map_err(|err: crate::events::EventError| err.to_string())
}

pub(crate) async fn sleep_unless_shutdown(state: &ServerState, delay: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = state.shutdown_requested() => false,
    }
}

pub(crate) fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    RETRY_START.saturating_mul(1 << doublings).min(RETRY_CAP)
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
            Feed::LedgerPush,
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
        Feed::LedgerPush,
        endpoint,
        status,
        message,
        retry_in,
        no_credential,
    );
}

/// Publishes a `ledger_push` event for a failed pass (R13, R9).
fn report_failure(state: &ServerState, error: &PushError, attempt: u32, retry_in: Duration) {
    let retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX);
    if let PushError::Moved {
        endpoint,
        status,
        location,
    } = error
    {
        let detail = json!({
            "status": "moved",
            "endpoint": endpoint,
            "http_status": status,
            "location": location,
            "hint": send::moved_hint(location),
            "attempt": attempt,
            "retry_in_ms": retry_in_ms,
            "error": format!("HTTP {status}: moved to {location} (redirect not followed)"),
        });
        state.events.publish(EventKind::LedgerPush, None, detail);
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
    let detail = json!({
        "status": "failed",
        "endpoint": endpoint,
        "http_status": status,
        "attempt": attempt,
        "retry_in_ms": retry_in_ms,
        "error": message,
    });
    state.events.publish(EventKind::LedgerPush, None, detail);
}

/// Publishes a `ledger_push` event when the endpoint follows a same-origin
/// 307/308 redirect to `location` (#446).
fn report_moved(events: &EventLog, endpoint: &str, http_status: u16, location: &str) {
    let detail = json!({
        "status": "moved",
        "endpoint": endpoint,
        "http_status": http_status,
        "location": location,
        "hint": send::moved_hint(location),
    });
    events.publish(EventKind::LedgerPush, None, detail);
}

/// Publishes a `ledger_push` event when the endpoint answers 410, once,
/// since the push then stops calling it.
fn report_stopped(state: &ServerState, endpoint: &str) {
    let detail = json!({
        "status": "stopped",
        "endpoint": endpoint,
        "http_status": 410,
        "error": STOPPED_MESSAGE,
    });
    state.events.publish(EventKind::LedgerPush, None, detail);
}

/// Publishes a `ledger_push` event when the push detects divergence (HTTP 409
/// `source_changed` or Collapse ahead of local ledgers), once, since the push
/// then pauses until `rosen collapse link --reset` (#445).
fn report_diverged(state: &ServerState, endpoint: &str, status: Option<u16>, message: &str) {
    let detail = json!({
        "status": "diverged",
        "endpoint": endpoint,
        "http_status": status,
        "error": message,
    });
    state.events.publish(EventKind::LedgerPush, None, detail);
}

/// Publishes a `ledger_push` event when a single ledger entry is refused by
/// the endpoint (HTTP 400, 413 or 422) and skipped so the rest of the batch
/// can be delivered (R13, #444).
fn report_refused(
    events: &EventLog,
    endpoint: &str,
    document: &str,
    ledger: &str,
    sequence: u64,
    http_status: u16,
    error: &str,
) {
    let detail = json!({
        "status": "refused",
        "endpoint": endpoint,
        "document": document,
        "ledger": ledger,
        "sequence": sequence,
        "http_status": http_status,
        "error": error,
    });
    events.publish(EventKind::LedgerPush, None, detail);
}

/// Publishes a `ledger_push` event when a pass succeeds after failures, so
/// whoever saw the failure knows it is over.
fn report_recovered(state: &ServerState, pushed: &Pushed, failures: u32) {
    let Pushed::Current {
        endpoint,
        sent,
        held,
    } = pushed
    else {
        return;
    };
    let detail = json!({
        "status": "sent",
        "endpoint": endpoint,
        "sent": sent,
        "held": { "discovery": held.discovery, "past_work": held.past_work },
        "after_failures": failures,
    });
    state.events.publish(EventKind::LedgerPush, None, detail);
}
