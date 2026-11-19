//! The ledger push (SPEC R13): after a Discoveries or Past work entry is
//! appended, the server sends it to the endpoint the base's `rosen.toml`
//! names in `ledger_endpoint`, such as Collapse's read-only copy of the
//! ledgers.
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
mod entries;
mod send;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rosen_protocol::EventKind;
use rosen_store::LooseObjectStore;
use serde_json::json;
use tokio::sync::watch;

pub use cursor::{Cursor, Cursors, Held, PerLedger};
pub use entries::{rfc3339_utc, MAX_BODY, MAX_ENTRIES};
pub use send::SendFailure;

use crate::landing::Ledger;
use crate::project_config;
use crate::{ServerConfig, ServerState};
use cursor::CursorStore;

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
    /// The endpoint answered 410 earlier; nothing is sent until it changes.
    Stopped { endpoint: String },
    /// The endpoint holds every entry; `sent` entries went in this pass.
    Current {
        endpoint: String,
        sent: usize,
        held: Held,
    },
}

/// Why a pass failed. Every failure but [`PushError::Gone`] is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Reading or writing the metadata store failed.
    Store(String),
    /// The endpoint's repository is archived or deleted (HTTP 410).
    Gone { endpoint: String },
    Send {
        endpoint: String,
        status: Option<u16>,
        message: String,
    },
}

/// What the task remembers between passes.
#[derive(Debug, Default)]
pub struct Session {
    /// The endpoint whose holdings this run has read, if any. Cleared by a
    /// failure, so the next pass asks again what the endpoint holds and
    /// looks its credential up again.
    reconciled: Option<Reconciled>,
    /// The endpoint that answered 410.
    stopped: Option<String>,
}

/// An endpoint this run has reached, with the credential found for it. The
/// credential is kept because a lookup can take seconds: Git Credential
/// Manager probes an unfamiliar host before it answers.
#[derive(Debug)]
struct Reconciled {
    endpoint: String,
    credential: Option<send::Credential>,
}

/// Sends new ledger entries until shutdown is requested.
pub async fn run(state: Arc<ServerState>) {
    let mut wake = state.events.watch();
    let mut session = Session::default();
    let mut failures: u32 = 0;
    loop {
        // Appends after this point wake the next pass, even if they arrive
        // while this one is sending.
        let seen = state.events.announced_seq();
        let keep_going = match push_once(&state, &mut session).await {
            Ok(pushed) => {
                if failures > 0 {
                    report_recovered(&state, &pushed, failures);
                }
                failures = 0;
                // Without an endpoint only a land can change the outcome.
                let sending = matches!(pushed, Pushed::Current { .. });
                wait_for_news(&state, &mut wake, seen, sending).await
            }
            Err(PushError::Gone { endpoint }) => {
                report_stopped(&state, &endpoint);
                session.stopped = Some(endpoint);
                failures = 0;
                wait_for_news(&state, &mut wake, seen, false).await
            }
            Err(error) => {
                session.reconciled = None;
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

/// Brings the endpoint up to the ledgers once.
pub async fn push_once(state: &ServerState, session: &mut Session) -> Result<Pushed, PushError> {
    let Some(endpoint) = configured_endpoint(&state.config).await? else {
        return Ok(Pushed::NotConfigured);
    };
    if session.stopped.as_deref() == Some(endpoint.as_str()) {
        return Ok(Pushed::Stopped { endpoint });
    }
    session.stopped = None;
    push_to(&state.config, &endpoint, session).await
}

/// Sends every entry `endpoint` lacks, one batch at a time. Public so a
/// test can aim it at a local endpoint without a project configuration.
pub async fn push_to(
    config: &ServerConfig,
    endpoint: &str,
    session: &mut Session,
) -> Result<Pushed, PushError> {
    let fresh = !matches!(&session.reconciled, Some(known) if known.endpoint == endpoint);
    let credential = match &session.reconciled {
        Some(known) if !fresh => known.credential.clone(),
        _ => send::credential(&config.data_dir, endpoint).await,
    };
    let target = Target::new(config, endpoint, credential);
    let mut held = Held::default();
    if fresh {
        held = target.send(entries::empty_body()).await?;
        target
            .update(move |cursors| {
                cursors.reconcile(&held);
                true
            })
            .await?;
        session.reconciled = Some(Reconciled {
            endpoint: endpoint.to_owned(),
            credential: target.credential.clone(),
        });
    }
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
        let kept = if batch.count == 0 {
            // Only skipped rows: the cursor moves past them, nothing is sent.
            target.update(move |cursors| cursors.skip(&extent)).await?
        } else {
            held = target.send(batch.body).await?;
            sent += batch.count;
            target
                .update(move |cursors| cursors.acknowledge(&extent, &held))
                .await?
        };
        if !kept {
            return Err(target.failure(None, lost_batch(&extent, &held)));
        }
    }
}

/// One endpoint and what reaching it needs.
struct Target {
    endpoint: String,
    metadata: PathBuf,
    scratch: PathBuf,
    credential: Option<send::Credential>,
}

impl Target {
    fn new(config: &ServerConfig, endpoint: &str, credential: Option<send::Credential>) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            metadata: config.metadata_path(),
            scratch: config.data_dir.join("ledger-push"),
            credential,
        }
    }

    async fn send(&self, body: Vec<u8>) -> Result<Held, PushError> {
        let credential = self.credential.as_ref();
        send::post(&self.scratch, &self.endpoint, &body, credential)
            .await
            .map_err(|failure| match failure {
                SendFailure::Gone => PushError::Gone {
                    endpoint: self.endpoint.clone(),
                },
                SendFailure::Failed { status, message } => self.failure(status, message),
            })
    }

    fn failure(&self, status: Option<u16>, message: String) -> PushError {
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
        }
    }

    /// The next batch after the stored cursors, read on the blocking pool.
    async fn next_batch(&self) -> Result<entries::Batch, PushError> {
        let (metadata, endpoint) = (self.metadata.clone(), self.endpoint.clone());
        blocking(move || {
            let store = CursorStore::open(&metadata)?;
            let cursors = store.load(&endpoint)?;
            entries::next_batch(store.connection(), &cursors)
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
    let (metadata, objects) = (config.metadata_path(), config.objects_dir());
    let found = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
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
        Ok(in_force.config.ledger_endpoint.clone())
    })
    .await
    .map_err(|join| PushError::Store(join.to_string()))?;
    found.map_err(PushError::Store)
}

/// Waits until an event after `seen` can change what a pass would do: a
/// land, which can change the endpoint, or, while `sending`, a channel line,
/// which an append writes. Returns false when the server is shutting down.
async fn wait_for_news(
    state: &Arc<ServerState>,
    wake: &mut watch::Receiver<u64>,
    seen: u64,
    sending: bool,
) -> bool {
    let mut seen = seen;
    loop {
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

async fn sleep_unless_shutdown(state: &ServerState, delay: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = state.shutdown_requested() => false,
    }
}

fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    RETRY_START.saturating_mul(1 << doublings).min(RETRY_CAP)
}

/// Publishes a `ledger_push` event for a failed pass (R13, R9).
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

/// Publishes a `ledger_push` event when the endpoint answers 410, once,
/// since the push then stops calling it.
fn report_stopped(state: &ServerState, endpoint: &str) {
    let detail = json!({
        "status": "stopped",
        "endpoint": endpoint,
        "http_status": 410,
        "error": "the endpoint's repository is archived or deleted; nothing more is sent \
                  until ledger_endpoint changes or the server restarts",
    });
    state.events.publish(EventKind::LedgerPush, None, detail);
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
