//! The Collapse action relay (R14 "Collapse action relay"): the server
//! fetches the owner's approve, land, mark, configuration change and
//! machine revoke requests from its Collapse endpoint and runs them through
//! its normal RPCs, under an allow-list only the operator sets.
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

pub(crate) mod action;
mod config_change;
mod machine_revoke;
pub(crate) mod remote;
pub(crate) mod store;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rosen_protocol::{ErrorBody, ErrorCode, EventKind, Outcome, RelayKind, RelaySetting, Response};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::collapse_health::Feed;
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
    /// An unfollowed redirect (HTTP 301 or cross-origin 307/308) (#446).
    Moved {
        endpoint: String,
        status: u16,
        location: String,
    },
    Send {
        endpoint: String,
        status: Option<u16>,
        message: String,
        no_credential: bool,
    },
}

const STOPPED_MESSAGE: &str = "the endpoint's repository is archived or deleted; the relay calls \
                               it no more until collapse changes or the server restarts";

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
    /// Followed same-origin 307/308 Collapse root for the session:
    /// `(configured_root, active_root)` (#446).
    redirected: Option<(String, String)>,
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
                state
                    .collapse
                    .record_stopped(Feed::Relay, &endpoint, STOPPED_MESSAGE);
                session.stopped = Some(endpoint);
                failures = 0;
                DOORBELL_EVERY
            }
            Err(error) => {
                // The token may have changed: look it up again next time.
                session.remote = None;
                failures = failures.saturating_add(1);
                let delay = retry_delay(failures).max(DOORBELL_EVERY);
                let root = session
                    .configured
                    .as_ref()
                    .and_then(|(root, _)| root.as_deref());
                report_failure(&state, &error, failures, delay);
                record_failure(&state, root, &error, delay);
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
    // Only the mirror owner talks to Collapse (#433): a copied data
    // directory keeps the relay allow-list and would otherwise run actions.
    if !crate::mirror::owner::is_owner_async(state.config.data_dir.clone()).await {
        session.remote = None;
        state.relay_wake.take_fetch();
        return Ok(());
    }
    let Some(root) = configured_root(state, session).await? else {
        return Ok(());
    };
    if session.stopped.as_deref() == Some(root.as_str()) {
        return Ok(());
    }
    session.stopped = None;
    let _in_flight = state.collapse.begin(Feed::Relay, &root);
    if !matches!(&session.remote, Some(remote) if remote.root == root) {
        session.remote = Some(connect(state, &root).await);
        session.recovered = false;
    }
    let mut active_root = match &session.redirected {
        Some((configured, target)) if configured == &root => target.clone(),
        _ => {
            session.redirected = None;
            root.clone()
        }
    };
    let remote = session.remote.as_ref().expect("connected above");
    if active_root == remote.root {
        if let Some(new_root) = state.collapse.moved_root_for(&remote.root) {
            note_redirect(state, &remote.root, &mut active_root, Some((307, new_root)));
        }
    }
    let outcome = run_step(state, remote, &mut active_root, &mut session.recovered).await;
    if active_root != root {
        session.redirected = Some((root, active_root));
    }
    outcome
}

async fn run_step(
    state: &Arc<ServerState>,
    remote: &Remote,
    active_root: &mut String,
    recovered: &mut bool,
) -> Result<(), RelayError> {
    post_unposted(state, remote, active_root).await?;
    let dir = state.config.data_dir.clone();
    let setting = blocking(move || RelayStore::open(&dir)?.setting())
        .await
        .map_err(RelayError::Store)?;
    if !*recovered {
        settle_interrupted(state, remote, active_root, &setting).await?;
        *recovered = true;
    }
    if !setting.active() {
        // A feed answer while off asks for nothing this server will run.
        state.relay_wake.take_fetch();
        state.collapse.record_idle(Feed::Relay, &remote.root, None);
        return Ok(());
    }
    fetch_if_rung(state, remote, active_root, &setting).await
}

/// Fetches and runs the queue when the doorbell's generation changed since
/// the last fetch, or a feed answer asked.
async fn fetch_if_rung(
    state: &Arc<ServerState>,
    remote: &Remote,
    active_root: &mut String,
    setting: &RelaySetting,
) -> Result<(), RelayError> {
    let forced = state.relay_wake.take_fetch();
    let rung = poll_doorbell(state, remote, active_root).await;
    let generation = match rung {
        Ok(generation) => {
            state.collapse.record_success(Feed::Relay, &remote.root);
            Some(generation)
        }
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
        fetch(state, remote, active_root, setting, cursor.after).await?;
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

/// Reads the doorbell, falling back to probing `/rosen/actions` for a
/// same-origin 307/308 when Collapse's edge doorbell Worker returns 404 for a
/// renamed repository (#446).
async fn poll_doorbell(
    state: &ServerState,
    remote: &Remote,
    active_root: &mut String,
) -> Result<i64, RelayError> {
    match remote.doorbell_at(active_root).await {
        Ok((generation, moved)) => {
            note_redirect(state, &remote.root, active_root, moved);
            Ok(generation)
        }
        Err(
            error @ RelayError::Send {
                status: Some(404), ..
            },
        ) if *active_root == remote.root => {
            if let Some((status, new_root)) = remote.probe_moved_root(active_root).await {
                note_redirect(state, &remote.root, active_root, Some((status, new_root)));
                let (generation, moved) = remote.doorbell_at(active_root).await?;
                note_redirect(state, &remote.root, active_root, moved);
                return Ok(generation);
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// Takes up every queued action after `after`, page by page.
async fn fetch(
    state: &Arc<ServerState>,
    remote: &Remote,
    active_root: &mut String,
    setting: &RelaySetting,
    mut after: i64,
) -> Result<(), RelayError> {
    loop {
        let (actions, moved) = remote.actions_at(active_root, after).await?;
        note_redirect(state, &remote.root, active_root, moved);
        state.collapse.record_success(Feed::Relay, &remote.root);
        if actions.is_empty() {
            return Ok(());
        }
        let mut highest = after;
        for raw in actions {
            highest = highest.max(take_up(state, remote, active_root, setting, raw).await?);
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
    active_root: &mut String,
    setting: &RelaySetting,
    raw: Value,
) -> Result<i64, RelayError> {
    let Some(fetched) = Fetched::parse(&raw) else {
        // Without an ID there is nowhere to send a refusal.
        let id = raw.get("id").and_then(Value::as_i64).unwrap_or(0);
        if id > 0 {
            let refused = action::refusal("", "invalid_action", "malformed action");
            record(state, remote, active_root, id, &raw, refused).await?;
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
    record(state, remote, active_root, id, &raw, done).await?;
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
    active_root: &mut String,
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
    post(state, remote, active_root, id, outcome, &result).await
}

async fn post(
    state: &Arc<ServerState>,
    remote: &Remote,
    active_root: &mut String,
    id: i64,
    outcome: &str,
    result: &str,
) -> Result<(), RelayError> {
    let (_, moved) = remote
        .post_result_at(active_root, id, outcome, result)
        .await?;
    note_redirect(state, &remote.root, active_root, moved);
    state.collapse.record_success(Feed::Relay, &remote.root);
    let root = remote.root.clone();
    with_store(state, move |store| store.posted(&root, id)).await
}

/// Re-posts every outcome Collapse has not accepted, as a stopped or
/// failed server left it.
async fn post_unposted(
    state: &Arc<ServerState>,
    remote: &Remote,
    active_root: &mut String,
) -> Result<(), RelayError> {
    let root = remote.root.clone();
    let now = crate::post_land::submitter::now_millis();
    let unposted = with_store(state, move |store| {
        store.prune(now)?;
        store.unposted(&root)
    })
    .await?;
    for outcome in unposted {
        post(
            state,
            remote,
            active_root,
            outcome.id,
            &outcome.state,
            &outcome.result,
        )
        .await?;
    }
    Ok(())
}

/// Settles actions a stopped server left running: the stored approval or
/// mark when it took effect, otherwise one run now, so none runs twice.
async fn settle_interrupted(
    state: &Arc<ServerState>,
    remote: &Remote,
    active_root: &mut String,
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
        record(state, remote, active_root, id, &raw, done).await?;
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

fn note_redirect(
    state: &ServerState,
    configured_root: &str,
    active_root: &mut String,
    moved: Option<(u16, String)>,
) {
    let Some((status, new_root)) = moved else {
        return;
    };
    if *active_root == new_root {
        return;
    }
    *active_root = new_root.clone();
    state
        .collapse
        .record_moved(Feed::Relay, configured_root, &new_root);
    report(
        state,
        json!({
            "status": "moved",
            "endpoint": configured_root,
            "http_status": status,
            "location": new_root,
            "hint": send::moved_hint(&new_root),
        }),
    );
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

fn record_failure(state: &ServerState, root: Option<&str>, error: &RelayError, retry_in: Duration) {
    if let RelayError::Moved {
        endpoint,
        status,
        location,
    } = error
    {
        let message = format!("HTTP {status}: moved to {location} (redirect not followed)");
        state.collapse.record_moved_unfollowed(
            Feed::Relay,
            root.unwrap_or(endpoint.as_str()),
            *status,
            location,
            &message,
            retry_in,
        );
        return;
    }
    let (endpoint, status, message, no_credential) = match error {
        RelayError::Store(message) => (root, None, message.as_str(), false),
        RelayError::Gone { endpoint } => (Some(endpoint.as_str()), Some(410), "gone", false),
        RelayError::Moved { .. } => unreachable!(),
        RelayError::Send {
            endpoint,
            status,
            message,
            no_credential,
        } => (
            root.or(Some(endpoint.as_str())),
            *status,
            message.as_str(),
            *no_credential,
        ),
    };
    state.collapse.record_failure(
        Feed::Relay,
        endpoint,
        status,
        message,
        retry_in,
        no_credential,
    );
}

/// Publishes a `relay` event for a failed step (R9, R14).
fn report_failure(state: &ServerState, error: &RelayError, attempt: u32, retry_in: Duration) {
    let retry_in_ms = u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX);
    if let RelayError::Moved {
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
        RelayError::Store(message) => (None, None, message.as_str()),
        RelayError::Gone { endpoint } => (Some(endpoint.as_str()), Some(410), "gone"),
        RelayError::Moved { .. } => unreachable!(),
        RelayError::Send {
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

fn report(state: &ServerState, detail: Value) {
    state.events.publish(EventKind::Relay, None, detail);
}
