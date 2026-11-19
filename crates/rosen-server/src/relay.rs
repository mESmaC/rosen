//! The Collapse action relay (R14 "Collapse action relay"): the server
//! fetches the owner's approve, land, mark and configuration change
//! requests from its Collapse endpoint and runs them through its normal
//! RPCs, under an allow-list only the operator sets.
//!
//! Collapse sits behind no NAT but the server does, so the server pulls.
//! It never polls Collapse's origin: every 20 s it reads the doorbell,
//! which Cloudflare's edge answers without waking Collapse's Machine, and
//! fetches the queue only when the doorbell's generation changed or a
//! state feed answer said `actions_pending`. An idle project costs
//! Collapse nothing but edge reads.
//!
//! One background task does all of it, off every request path, like the
//! state feed it shares its sender and credential with.

mod action;
mod config_change;
mod remote;
mod store;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rosen_protocol::{ErrorBody, ErrorCode, EventKind, Outcome, RelayKind, RelaySetting, Response};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::ledger_push::{retry_delay, send};
use crate::ServerState;
use action::{Call, Done};
use remote::{Fetched, Remote};
use store::{Cursor, RelayStore};

/// How often the doorbell is read while the relay is on.
const DOORBELL_EVERY: Duration = Duration::from_secs(20);

/// Wakes the relay task before its next doorbell read.
#[derive(Default)]
pub struct Wake {
    notify: Notify,
    fetch: AtomicBool,
}

impl Wake {
    /// A state feed answer said actions wait: fetch them now, whatever
    /// the doorbell says, since the edge can lag a ring by a minute.
    pub fn actions_pending(&self) {
        self.fetch.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    /// The setting changed: look at it again now.
    fn setting_changed(&self) {
        self.notify.notify_one();
    }

    fn take_fetch(&self) -> bool {
        self.fetch.swap(false, Ordering::SeqCst)
    }
}

/// Why a step failed. Every failure but [`RelayError::Gone`] is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayError {
    Store(String),
    /// The endpoint's repository is archived or deleted (HTTP 410).
    Gone {
        endpoint: String,
    },
    Send {
        endpoint: String,
        status: Option<u16>,
        message: String,
    },
}

/// The setting as stored, for `server status`; `None` if unreadable.
pub async fn setting(state: &Arc<ServerState>) -> Option<RelaySetting> {
    let dir = state.config.data_dir.clone();
    blocking(move || RelayStore::open(&dir)?.setting())
        .await
        .ok()
}

/// The setting for the state feed's `relay` document: the kinds Collapse
/// may offer. Reads SQLite: call it on the blocking pool.
pub fn offered(data_dir: &Path) -> rusqlite::Result<Vec<RelayKind>> {
    Ok(RelayStore::open(data_dir)?.setting()?.offered())
}

/// `rosen server relay --allow`: stores the allow-list, publishes a
/// `relay` event so the feed tells Collapse, and wakes the task.
pub async fn set(state: &Arc<ServerState>, allow: Vec<RelayKind>) -> Outcome {
    let dir = state.config.data_dir.clone();
    let now = crate::machines::now();
    match blocking(move || RelayStore::open(&dir)?.set(&allow, now)).await {
        Ok(setting) => {
            let detail = json!({ "status": "setting", "allow": setting.allow });
            state.events.publish(EventKind::Relay, None, detail);
            state.relay_wake.setting_changed();
            Outcome::Ok(Response::Relay(setting))
        }
        Err(error) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, error)),
    }
}

/// What the task remembers between steps.
#[derive(Default)]
struct Session {
    /// The Collapse root, and the event sequence it was read at; read
    /// again after any later land.
    configured: Option<(Option<String>, u64)>,
    remote: Option<Remote>,
    /// The endpoint that answered 410.
    stopped: Option<String>,
    /// Whether actions a stopped server left running were settled.
    recovered: bool,
}

/// Reads the doorbell and runs fetched actions until shutdown.
pub async fn run(state: Arc<ServerState>) {
    let mut session = Session::default();
    let mut failures: u32 = 0;
    loop {
        let wait = match step(&state, &mut session).await {
            Ok(()) => {
                if failures > 0 {
                    report(
                        &state,
                        json!({ "status": "recovered", "after_failures": failures }),
                    );
                    failures = 0;
                }
                DOORBELL_EVERY
            }
            Err(RelayError::Gone { endpoint }) => {
                report_stopped(&state, &endpoint);
                session.stopped = Some(endpoint);
                failures = 0;
                DOORBELL_EVERY
            }
            Err(error) => {
                // The token may have changed: look it up again next time.
                session.remote = None;
                failures = failures.saturating_add(1);
                let delay = retry_delay(failures).max(DOORBELL_EVERY);
                report_failure(&state, &error, failures, delay);
                delay
            }
        };
        let keep_going = tokio::select! {
            () = tokio::time::sleep(wait) => true,
            () = state.relay_wake.notify.notified() => true,
            () = state.shutdown_requested() => false,
        };
        if !keep_going {
            return;
        }
    }
}

/// Posts outcomes Collapse lacks, then, while the relay is on, reads the
/// doorbell and fetches and runs actions when it rang.
async fn step(state: &Arc<ServerState>, session: &mut Session) -> Result<(), RelayError> {
    let Some(root) = configured_root(state, session).await? else {
        return Ok(());
    };
    if session.stopped.as_deref() == Some(root.as_str()) {
        return Ok(());
    }
    session.stopped = None;
    if !matches!(&session.remote, Some(remote) if remote.root == root) {
        session.remote = Some(connect(state, &root).await);
        session.recovered = false;
    }
    let remote = session.remote.as_ref().expect("connected above");
    post_unposted(state, remote).await?;
    let dir = state.config.data_dir.clone();
    let setting = blocking(move || RelayStore::open(&dir)?.setting())
        .await
        .map_err(RelayError::Store)?;
    if !session.recovered {
        settle_interrupted(state, remote, &setting).await?;
        session.recovered = true;
    }
    if !setting.active() {
        // A feed answer while off asks for nothing this server will run.
        state.relay_wake.take_fetch();
        return Ok(());
    }
    fetch_if_rung(state, remote, &setting).await
}

/// Fetches and runs the queue when the doorbell's generation changed since
/// the last fetch, or a feed answer asked.
async fn fetch_if_rung(
    state: &Arc<ServerState>,
    remote: &Remote,
    setting: &RelaySetting,
) -> Result<(), RelayError> {
    let forced = state.relay_wake.take_fetch();
    let rung = remote.doorbell().await;
    let generation = match rung {
        Ok(generation) => Some(generation),
        // The feed said actions wait; the doorbell can catch up later.
        Err(_) if forced => None,
        Err(error) => return Err(error),
    };
    let cursor = with_store(state, {
        let root = remote.root.clone();
        move |store| store.cursor(&root)
    })
    .await?;
    if forced || generation != cursor.generation {
        fetch(state, remote, setting, cursor.after).await?;
    }
    if let Some(generation) = generation {
        let root = remote.root.clone();
        let saved = Cursor {
            generation: Some(generation),
            after: 0,
        };
        with_store(state, move |store| store.save_cursor(&root, saved)).await?;
    }
    Ok(())
}

/// Takes up every queued action after `after`, page by page.
async fn fetch(
    state: &Arc<ServerState>,
    remote: &Remote,
    setting: &RelaySetting,
    mut after: i64,
) -> Result<(), RelayError> {
    loop {
        let actions = remote.actions(after).await?;
        if actions.is_empty() {
            return Ok(());
        }
        let mut highest = after;
        for raw in actions {
            highest = highest.max(take_up(state, remote, setting, raw).await?);
        }
        if highest <= after {
            return Ok(());
        }
        after = highest;
        let root = remote.root.clone();
        // Only the action cursor moves here; the generation is saved once
        // the whole queue was read.
        with_store(state, move |store| {
            let generation = store.cursor(&root)?.generation;
            store.save_cursor(&root, Cursor { generation, after })
        })
        .await?;
    }
}

/// Runs one fetched action at most once and posts what it came to.
/// Returns its ID, or 0 for an entry with none.
async fn take_up(
    state: &Arc<ServerState>,
    remote: &Remote,
    setting: &RelaySetting,
    raw: Value,
) -> Result<i64, RelayError> {
    let Some(fetched) = Fetched::parse(&raw) else {
        // Without an ID there is nowhere to send a refusal.
        let id = raw.get("id").and_then(Value::as_i64).unwrap_or(0);
        if id > 0 {
            let refused = action::refusal("", "invalid_action", "malformed action");
            record(state, remote, id, &raw, refused).await?;
        }
        return Ok(id);
    };
    let id = fetched.id;
    let taken = with_store(state, {
        let root = remote.root.clone();
        move |store| store.taken(&root, id)
    })
    .await?;
    if taken.is_some() {
        // A repeat: Collapse hands out an action until it holds a result.
        // It runs nothing; an unposted outcome goes with the next step.
        return Ok(id);
    }
    if state.is_shutting_down() {
        // Left for the next start, where Collapse hands it out again.
        return Ok(0);
    }
    let now = crate::post_land::submitter::now_millis();
    let done = match action::check(&fetched, setting, now) {
        Err(refused) => refused,
        Ok(call) => {
            if !start(state, remote, id, &raw, now).await? {
                return Ok(id);
            }
            run_call(state, call).await
        }
    };
    record(state, remote, id, &raw, done).await?;
    Ok(id)
}

/// Marks `id` running before it runs, so it never runs twice.
async fn start(
    state: &Arc<ServerState>,
    remote: &Remote,
    id: i64,
    raw: &Value,
    now: i64,
) -> Result<bool, RelayError> {
    let (root, text) = (remote.root.clone(), raw.to_string());
    with_store(state, move |store| store.start(&root, id, &text, now)).await
}

async fn run_call(state: &Arc<ServerState>, call: Call) -> Done {
    // As every RPC does, so an operator history rewrite never runs under it.
    let _running = state.maintenance.read().await;
    action::run(state, call).await
}

/// Stores the outcome, reports it, and posts it to Collapse.
async fn record(
    state: &Arc<ServerState>,
    remote: &Remote,
    id: i64,
    raw: &Value,
    done: Done,
) -> Result<(), RelayError> {
    let (root, text, result) = (
        remote.root.clone(),
        raw.to_string(),
        done.result.to_string(),
    );
    let outcome = done.state;
    let now = crate::post_land::submitter::now_millis();
    let stored = result.clone();
    with_store(state, move |store| {
        store.start(&root, id, &text, now)?;
        store.finish(&root, id, outcome, &stored)?;
        store.touch_machine(crate::machines::now())
    })
    .await?;
    report(
        state,
        json!({
            "status": outcome,
            "action": id,
            "kind": raw.get("kind"),
            "account": raw.get("account"),
            "code": done.result.get("code"),
        }),
    );
    post(state, remote, id, outcome, &result).await
}

async fn post(
    state: &Arc<ServerState>,
    remote: &Remote,
    id: i64,
    outcome: &str,
    result: &str,
) -> Result<(), RelayError> {
    remote.post_result(id, outcome, result).await?;
    let root = remote.root.clone();
    with_store(state, move |store| store.posted(&root, id)).await
}

/// Re-posts every outcome Collapse has not accepted, as a stopped or
/// failed server left it.
async fn post_unposted(state: &Arc<ServerState>, remote: &Remote) -> Result<(), RelayError> {
    let root = remote.root.clone();
    let now = crate::post_land::submitter::now_millis();
    let unposted = with_store(state, move |store| {
        store.prune(now)?;
        store.unposted(&root)
    })
    .await?;
    for outcome in unposted {
        post(state, remote, outcome.id, &outcome.state, &outcome.result).await?;
    }
    Ok(())
}

/// Settles actions a stopped server left running: the stored approval or
/// mark when it took effect, otherwise one run now, so none runs twice.
async fn settle_interrupted(
    state: &Arc<ServerState>,
    remote: &Remote,
    setting: &RelaySetting,
) -> Result<(), RelayError> {
    let root = remote.root.clone();
    let interrupted = with_store(state, move |store| store.interrupted(&root)).await?;
    for (id, text, started_at) in interrupted {
        let raw: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let done = match Fetched::parse(&raw) {
            Some(fetched) => settle(state, &fetched, setting, started_at).await,
            None => action::refusal("", "invalid_action", "malformed action"),
        };
        record(state, remote, id, &raw, done).await?;
    }
    Ok(())
}

/// One interrupted action: what it stored if it took effect, otherwise
/// one run now if the setting still allows it. It is checked as when it
/// started, so expiry since then does not count.
async fn settle(
    state: &Arc<ServerState>,
    fetched: &Fetched,
    setting: &RelaySetting,
    started_at: i64,
) -> Done {
    let any = RelaySetting {
        allow: RelayKind::ALL.to_vec(),
        revoked: false,
    };
    let call = match action::check(fetched, &any, started_at) {
        Ok(call) => call,
        Err(refused) => return refused,
    };
    if let Some(done) = action::recover(state, &call, started_at).await {
        return done;
    }
    match action::check(fetched, setting, started_at) {
        Ok(call) => run_call(state, call).await,
        Err(refused) => refused,
    }
}

/// The base's `collapse` root, read again only after a land.
async fn configured_root(
    state: &Arc<ServerState>,
    session: &mut Session,
) -> Result<Option<String>, RelayError> {
    if let Some((root, read_at)) = &session.configured {
        let (root, read_at) = (root.clone(), *read_at);
        let reading = Arc::clone(state);
        let landed =
            blocking(move || Ok(reading.events.holds_kind_after(read_at, EventKind::Landed)))
                .await
                .map_err(RelayError::Store)?
                .map_err(|error| RelayError::Store(error.to_string()))?;
        if !landed {
            return Ok(root);
        }
    }
    let read_at = state.events.announced_seq();
    let in_force = crate::ledger_push::in_force(&state.config)
        .await
        .map_err(RelayError::Store)?;
    let root = in_force.and_then(|config| config.collapse_url("").map(trimmed));
    session.configured = Some((root.clone(), read_at));
    Ok(root)
}

fn trimmed(url: String) -> String {
    url.trim_end_matches('/').to_owned()
}

/// The endpoint with the token the state feed uses for it.
async fn connect(state: &Arc<ServerState>, root: &str) -> Remote {
    let credential = send::credential(&state.config.data_dir, &format!("{root}/rosen/state")).await;
    Remote {
        root: root.to_owned(),
        credential,
        scratch: state.config.data_dir.join("relay"),
    }
}

async fn with_store<T: Send + 'static>(
    state: &ServerState,
    work: impl FnOnce(&RelayStore) -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, RelayError> {
    let dir = state.config.data_dir.clone();
    blocking(move || work(&RelayStore::open(&dir)?))
        .await
        .map_err(RelayError::Store)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|join| join.to_string())?
        .map_err(|error| error.to_string())
}

/// Publishes a `relay` event for a failed step (R9, R14).
fn report_failure(state: &ServerState, error: &RelayError, attempt: u32, retry_in: Duration) {
    let retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX);
    let (endpoint, status, message) = match error {
        RelayError::Store(message) => (None, None, message.as_str()),
        RelayError::Gone { endpoint } => (Some(endpoint.as_str()), Some(410), "gone"),
        RelayError::Send {
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

fn report_stopped(state: &ServerState, endpoint: &str) {
    report(
        state,
        json!({
            "status": "stopped",
            "endpoint": endpoint,
            "http_status": 410,
            "error": "the endpoint's repository is archived or deleted; the relay calls it \
                      no more until collapse changes or the server restarts",
        }),
    );
}

fn report(state: &ServerState, detail: Value) {
    state.events.publish(EventKind::Relay, None, detail);
}
