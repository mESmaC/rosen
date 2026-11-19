//! One send: build the dirty documents, drop those the endpoint already
//! holds, and post the rest in batches, moving the cursor only as the
//! endpoint acknowledges them.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use rosen_core::{ChangeId, ChangeState};
use rosen_protocol::ChangeFilter;

use super::batch;
use super::dirty::Pending;
use super::facts::Stores;
use super::review_docs;
use super::store::{Cursor, StateStore};
use super::unit::Unit;
use super::{change_doc, PushError};
use crate::ledger_push::send::{self, Credential, SendFailure};
use crate::ServerState;

/// One endpoint and what reaching it needs.
pub struct Target {
    pub endpoint: String,
    pub scratch: PathBuf,
    pub credential: Option<Credential>,
    pub stores: Stores,
}

impl Target {
    /// Posts `body` and returns the highest event sequence the endpoint
    /// holds.
    pub async fn post(&self, body: Vec<u8>) -> Result<u64, PushError> {
        let credential = self.credential.as_ref();
        let accepted = send::post_json(&self.scratch, &self.endpoint, &body, credential)
            .await
            .map_err(|failure| self.failure(failure))?;
        // `{"highest_sequence":N}`
        let answer: serde_json::Value = serde_json::from_str(&accepted.body)
            .map_err(|_| self.failure(accepted.unreadable("unreadable answer")))?;
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

/// Sends what `pending` names through its scanned sequence. Returns how
/// many documents went.
pub async fn send(
    state: &Arc<ServerState>,
    target: &Target,
    pending: &Pending,
    cursor: &mut Cursor,
) -> Result<usize, PushError> {
    let seq = pending.scanned_to;
    let units = {
        // An operator rewrite of history must not run under a half-read pass.
        let _running = state.maintenance.read().await;
        build(state, target, pending).await?
    };
    let everything = pending.everything;
    let units = target
        .with_store(move |store, endpoint| unsent(store, endpoint, units, everything))
        .await?;
    let batches = batch::split(units, seq, cursor.delivered);
    let count = batches.len();
    let mut sent = 0;
    for (index, batch) in batches.into_iter().enumerate() {
        let last = index + 1 == count;
        let through = if last { seq } else { cursor.delivered };
        let held = target.post(batch.body).await?;
        if held < through {
            return Err(lost(target, cursor, through, held).await);
        }
        let mut next = *cursor;
        if last {
            next = Cursor {
                scanned: seq,
                delivered: seq.max(cursor.delivered),
            };
        }
        sent += batch.documents.len();
        let documents = batch.documents;
        target
            .with_store(move |s, e| s.delivered(e, &documents, next))
            .await?;
        *cursor = next;
    }
    if count == 0 {
        // Nothing changed for the endpoint; only the scan moves on.
        cursor.scanned = cursor.scanned.max(seq);
        let kept = *cursor;
        target
            .with_store(move |s, e| s.save_cursor(e, kept))
            .await?;
    }
    Ok(sent)
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
/// everything.
fn unsent(
    store: &mut StateStore,
    endpoint: &str,
    units: Vec<Unit>,
    everything: bool,
) -> rusqlite::Result<Vec<Unit>> {
    let mut kept = Vec::with_capacity(units.len());
    for unit in units {
        let held = store.sent(endpoint, &unit.document)?;
        if everything || held.is_none_or(|sent| sent.fingerprint != unit.fingerprint) {
            kept.push(unit);
        }
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
    Ok(units)
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
