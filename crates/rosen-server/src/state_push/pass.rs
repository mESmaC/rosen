//! One send: build the dirty documents, drop those the endpoint already
//! holds, and post the rest in batches, moving the cursor only as the
//! endpoint acknowledges them.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use rosen_core::{ChangeId, ChangeState};
use rosen_protocol::ChangeFilter;

use super::batch::{self, Batch, BatchItem};
use super::dirty::Pending;
use super::facts::Stores;
use super::store::{Cursor, Refusal, Sent, StateStore};
use super::unit::{Delivery, Section, Unit};
use super::{change_doc, PushError};
use super::{log_lines, machines_doc, platforms_doc, review_docs};
use crate::collapse_health::Feed;
use crate::ledger_push::send::{self, Credential, SendFailure};
use crate::ServerState;

/// One endpoint and what reaching it needs.
pub struct Target {
    pub endpoint: String,
    pub scratch: PathBuf,
    pub credential: Option<Credential>,
    pub stores: Stores,
    /// Told when an answer says owner actions wait in Collapse (R14).
    pub relay: Arc<crate::relay::Wake>,
}

impl Target {
    /// Posts `body` and returns the highest event sequence the endpoint
    /// holds.
    pub async fn post(&self, body: Vec<u8>) -> Result<u64, PushError> {
        let credential = self.credential.as_ref();
        let accepted = send::post_json(&self.scratch, &self.endpoint, &body, credential)
            .await
            .map_err(|failure| self.failure(failure))?;
        // `{"highest_sequence":N,"actions_pending":B}`
        let answer: serde_json::Value = serde_json::from_str(&accepted.body)
            .map_err(|_| self.failure(accepted.unreadable("unreadable answer")))?;
        if answer.get("actions_pending") == Some(&serde_json::Value::Bool(true)) {
            self.relay.actions_pending();
        }
        answer
            .get("highest_sequence")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| self.failure(accepted.unreadable("answer names no highest_sequence")))
    }

    pub fn failure(&self, failure: SendFailure) -> PushError {
        let endpoint = self.endpoint.clone();
        match failure {
            SendFailure::Gone => PushError::Gone { endpoint },
            SendFailure::Failed { status, message } => {
                let no_credential = endpoint.starts_with("https://")
                    && self.credential.is_none()
                    && matches!(status, Some(401 | 403));
                let message = match (&self.credential, status) {
                    (None, Some(401 | 403)) => {
                        format!("{message} (git's credential helper holds no token for {endpoint})")
                    }
                    _ => message,
                };
                PushError::Send {
                    endpoint,
                    status,
                    message,
                    no_credential,
                }
            }
        }
    }

    async fn with_store<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut StateStore, &str) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, PushError> {
        let (path, endpoint) = (self.stores.metadata().to_owned(), self.endpoint.clone());
        super::blocking(move || {
            let mut store = StateStore::open(&path)?;
            work(&mut store, &endpoint)
        })
        .await
    }
}

/// What one pass delivered.
#[derive(Debug, Default)]
pub struct Delivered {
    /// Documents and lines acknowledged.
    pub count: usize,
    /// Whether any batch was accepted by the endpoint.
    pub any_accepted: bool,
    /// Whether the presence document went and was accepted.
    pub presence: bool,
}

/// Per-unit tracking across size-split batches and 400/413/422 halving.
struct UnitProgress {
    document: String,
    fingerprint: String,
    related: BTreeSet<String>,
    delivery: Delivery,
    remaining: usize,
    refused: bool,
}

impl UnitProgress {
    fn from_units(units: &[Unit]) -> Vec<Self> {
        units
            .iter()
            .map(|unit| Self {
                document: unit.document.clone(),
                fingerprint: unit.fingerprint.clone(),
                related: unit.related.clone(),
                delivery: unit.delivery,
                remaining: unit.items.len(),
                refused: false,
            })
            .collect()
    }
}

/// Sends what `pending` names through its scanned sequence; `channel` says
/// whether channel lines go too.
pub async fn send(
    state: &Arc<ServerState>,
    target: &Target,
    pending: &Pending,
    cursor: &mut Cursor,
    channel: bool,
) -> Result<Delivered, PushError> {
    let seq = pending.scanned_to;
    let units = {
        // An operator rewrite of history must not run under a half-read pass.
        let _running = state.maintenance.read().await;
        let mut units = build(state, target, pending).await?;
        units.extend(extras(state, target, pending, cursor.scanned, channel).await?);
        units
    };
    let everything = pending.everything;
    let units = target
        .with_store(move |store, endpoint| unsent(store, endpoint, units, everything))
        .await?;
    let mut progress = UnitProgress::from_units(&units);
    let batches = batch::split_slice(&units, seq, cursor.delivered);
    let delivered = send_batches(state, target, batches, &mut progress, cursor, seq).await?;
    if cursor.scanned < seq {
        // Nothing went, or the pass's last item was refused: move only scan.
        cursor.scanned = cursor.scanned.max(seq);
        let kept = *cursor;
        target
            .with_store(move |s, e| s.save_cursor(e, kept))
            .await?;
    }
    Ok(delivered)
}

/// Posts `batches` in order. On HTTP 400, 413 or 422, splits a multi-item
/// batch by halves so refused items are isolated and skipped while valid items
/// still deliver (#444).
async fn send_batches(
    state: &Arc<ServerState>,
    target: &Target,
    batches: Vec<Batch>,
    progress: &mut [UnitProgress],
    cursor: &mut Cursor,
    seq: u64,
) -> Result<Delivered, PushError> {
    let count = batches.len();
    let mut delivered = Delivered::default();
    let mut stack: Vec<(Vec<BatchItem>, Option<Vec<u8>>, bool)> = batches
        .into_iter()
        .enumerate()
        .rev()
        .map(|(index, batch)| (batch.items, Some(batch.body), index + 1 == count))
        .collect();
    while let Some((items, prebuilt, is_final)) = stack.pop() {
        let through = if is_final { seq } else { cursor.delivered };
        let body = prebuilt.unwrap_or_else(|| batch::encode(through, &items));
        match target.post(body).await {
            Ok(held) => {
                if held < through {
                    return Err(lost(target, cursor, through, held).await);
                }
                state
                    .collapse
                    .record_success(Feed::StateFeed, &target.endpoint);
                record_accepted(
                    target,
                    progress,
                    &items,
                    cursor,
                    seq,
                    is_final,
                    &mut delivered,
                )
                .await?;
            }
            Err(PushError::Send { status, .. })
                if send::is_batch_refusal(status) && items.len() > 1 =>
            {
                let mut left = items;
                let right = left.split_off(left.len() / 2);
                stack.push((right, None, is_final));
                stack.push((left, None, false));
            }
            Err(PushError::Send {
                status: Some(status),
                message,
                ..
            }) if send::is_batch_refusal(Some(status)) => {
                record_refused(state, target, progress, &items[0], status, &message).await?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(delivered)
}

async fn record_accepted(
    target: &Target,
    progress: &mut [UnitProgress],
    items: &[BatchItem],
    cursor: &mut Cursor,
    seq: u64,
    is_final: bool,
    delivered: &mut Delivered,
) -> Result<(), PushError> {
    let mut documents = Vec::new();
    let mut lines = 0usize;
    for item in items {
        if item.section == Section::Presence {
            delivered.presence = true;
        }
        let unit = &mut progress[item.unit];
        unit.remaining = unit.remaining.saturating_sub(1);
        if unit.delivery == Delivery::Log {
            lines += 1;
        } else if unit.remaining == 0 && !unit.refused {
            let sent = Sent {
                fingerprint: unit.fingerprint.clone(),
                related: unit.related.clone(),
            };
            documents.push((unit.document.clone(), sent));
        }
    }
    let next = if is_final {
        Cursor {
            scanned: seq.max(cursor.scanned),
            delivered: seq.max(cursor.delivered),
        }
    } else {
        *cursor
    };
    delivered.any_accepted = true;
    delivered.count += documents.len() + lines;
    target
        .with_store(move |s, e| s.delivered(e, &documents, next))
        .await?;
    *cursor = next;
    Ok(())
}

async fn record_refused(
    state: &Arc<ServerState>,
    target: &Target,
    progress: &mut [UnitProgress],
    item: &BatchItem,
    status: u16,
    message: &str,
) -> Result<(), PushError> {
    let unit = &mut progress[item.unit];
    unit.refused = true;
    unit.remaining = unit.remaining.saturating_sub(1);
    let refusal = Refusal {
        document: unit.document.clone(),
        fingerprint: unit.fingerprint.clone(),
        http_status: status,
        message: message.to_owned(),
        at: now_millis(),
    };
    let document = refusal.document.clone();
    target
        .with_store(move |s, e| s.record_refused(e, &refusal))
        .await?;
    super::report_refused(
        state,
        &target.endpoint,
        &document,
        item.section,
        status,
        message,
    );
    Ok(())
}

/// The endpoint acknowledged a batch but holds less than it covered: it
/// lost documents. The cursor goes back to what it holds, and the failure
/// makes the next pass resend everything live.
async fn lost(target: &Target, cursor: &mut Cursor, through: u64, held: u64) -> PushError {
    cursor.reconcile(held);
    let kept = *cursor;
    if let Err(error) = target.with_store(move |s, e| s.save_cursor(e, kept)).await {
        return error;
    }
    let message = format!(
        "the endpoint accepted a batch through event {through} but reports holding only \
         {held}; resending from there"
    );
    target.failure(SendFailure::Failed {
        status: None,
        message,
    })
}

/// The units the endpoint does not hold yet; every one when resending
/// everything. Log lines always go unless previously refused. Documents
/// refused by the endpoint are skipped until their fingerprint changes. An
/// unchanged refresh goes only when something else does, so refreshing never
/// makes a request of its own.
fn unsent(
    store: &mut StateStore,
    endpoint: &str,
    units: Vec<Unit>,
    everything: bool,
) -> rusqlite::Result<Vec<Unit>> {
    let mut kept = Vec::with_capacity(units.len());
    let mut refreshed = Vec::new();
    for unit in units {
        if store.is_refused(endpoint, &unit.document, &unit.fingerprint)? {
            continue;
        }
        if unit.delivery == Delivery::Log {
            kept.push(unit);
            continue;
        }
        let held = store.sent(endpoint, &unit.document)?;
        if everything || held.is_none_or(|sent| sent.fingerprint != unit.fingerprint) {
            kept.push(unit);
        } else if unit.delivery == Delivery::Refresh {
            refreshed.push(unit);
        }
    }
    if !kept.is_empty() {
        kept.extend(refreshed);
    }
    Ok(kept)
}

/// Builds every document `pending` makes dirty, with the changes the
/// dirty ones overlap, or overlapped when last sent.
async fn build(
    state: &Arc<ServerState>,
    target: &Target,
    pending: &Pending,
) -> Result<Vec<Unit>, PushError> {
    let mut ids = pending.changes.clone();
    if pending.everything {
        ids.extend(live_changes(state).await?);
    }
    let asked: Vec<String> = ids.iter().map(|id| format!("change:{id}")).collect();
    let earlier = target
        .with_store(move |store, endpoint| {
            let mut related = BTreeSet::new();
            for document in &asked {
                if let Some(sent) = store.sent(endpoint, document)? {
                    related.extend(sent.related);
                }
            }
            Ok(related)
        })
        .await?;
    ids.extend(earlier.iter().filter_map(|id| id.parse::<ChangeId>().ok()));
    let mut units = Vec::with_capacity(ids.len());
    for id in &ids {
        units.push(change_unit(state, &target.stores, *id).await);
    }
    let overlapping: BTreeSet<ChangeId> = units
        .iter()
        .flat_map(|unit| unit.related.iter())
        .filter_map(|id| id.parse().ok())
        .filter(|id| !ids.contains(id))
        .collect();
    for id in overlapping {
        units.push(change_unit(state, &target.stores, id).await);
    }
    if pending.coverage {
        units.extend(coverage(state, target).await?);
    }
    if pending.config {
        units.extend(review_docs::config(state).await.map_err(PushError::Store)?);
    }
    if pending.platforms {
        units.extend(
            platforms_doc::build(state)
                .await
                .map_err(PushError::Store)?,
        );
    }
    Ok(units)
}

/// The machine list, presence, and the event and channel lines after
/// `after`, as `pending` asks.
async fn extras(
    state: &Arc<ServerState>,
    target: &Target,
    pending: &Pending,
    after: u64,
    channel: bool,
) -> Result<Vec<Unit>, PushError> {
    let metadata = target.stores.metadata().to_owned();
    let through = pending.scanned_to;
    let everything = pending.everything;
    let lines = pending.log || everything;
    let machines = (pending.machines || everything).then(|| running_agents(state));
    let presence = pending.presence || pending.refresh_presence || everything;
    let refresh = pending.refresh_presence;
    let relay = (pending.relay || everything).then(|| state.config.data_dir.clone());
    super::blocking(move || {
        let mut units = Vec::new();
        if let Some(data_dir) = relay {
            units.push(relay_unit(&data_dir)?);
        }
        if let Some(running) = machines {
            units.push(machines_doc::machines(
                &metadata,
                &running,
                crate::machines::now(),
            )?);
        }
        if presence {
            units.push(machines_doc::presence(&metadata, now_millis(), refresh)?);
        }
        if lines {
            units.extend(log_lines::read(&metadata, after, through, channel)?);
        }
        Ok(units)
    })
    .await
}

/// The relay document: the kinds Collapse may offer its owner (R14).
pub(crate) fn relay_unit(data_dir: &std::path::Path) -> rusqlite::Result<Unit> {
    let allow = crate::relay::offered(data_dir)?;
    let item = serde_json::json!({ "allow": allow });
    Ok(Unit::new("relay".to_owned(), vec![(Section::Relay, item)]))
}

/// A copy of the server's count of subscribed agents per machine.
pub fn running_agents(state: &ServerState) -> crate::machines::manage::Presence {
    state
        .machine_agents
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

async fn change_unit(state: &Arc<ServerState>, stores: &Stores, id: ChangeId) -> Unit {
    change_doc::build(state, stores, id)
        .await
        .unwrap_or_else(|why| change_doc::unreadable(id, &why))
}

/// Coverage rows, and a removal for each one sent earlier that a revert
/// took off the list.
async fn coverage(state: &Arc<ServerState>, target: &Target) -> Result<Vec<Unit>, PushError> {
    let mut units = review_docs::coverage(state)
        .await
        .map_err(PushError::Store)?;
    let listed: BTreeSet<String> = units.iter().map(|unit| unit.document.clone()).collect();
    let sent = target
        .with_store(|store, endpoint| store.documents(endpoint, "coverage:"))
        .await?;
    for (document, fingerprint) in sent {
        let removal = review_docs::coverage_removed(&document);
        if !listed.contains(&document) && fingerprint != removal.fingerprint {
            units.push(removal);
        }
    }
    Ok(units)
}

/// Every open and ready change.
async fn live_changes(state: &Arc<ServerState>) -> Result<Vec<ChangeId>, PushError> {
    let state = Arc::clone(state);
    let listed = tokio::task::spawn_blocking(move || {
        let mut ids = Vec::new();
        for wanted in [ChangeState::Open, ChangeState::Ready] {
            let filter = ChangeFilter {
                owner: None,
                state: Some(wanted),
            };
            let records = state.changes.list(&filter)?;
            ids.extend(records.into_iter().map(|record| record.change.id));
        }
        Ok::<_, crate::changes::ChangeError>(ids)
    })
    .await
    .map_err(|join| PushError::Store(join.to_string()))?;
    listed.map_err(|error| PushError::Store(error.to_string()))
}
