//! Path claims, shares and waits (SPEC R6). They are also AIRC's file claims
//! (R13): one record per path, held in the metadata store with the changes.
//!
//! The first-claim-wins rule is pure logic in `rosen_core::claim`; [`store`]
//! runs it inside one SQLite transaction per operation. Events are published
//! only after that transaction commits, so a subscriber never sees a claim or
//! a wake that a rollback undid.

mod store;

pub use store::ensure_schema;

use crate::changes::ChangeError;
use crate::ServerState;
use rosen_core::ChangeId;
use rosen_protocol::{
    ClaimOutcome, ClaimRequest, ErrorBody, ErrorCode, EventKind, Outcome, ReleaseOutcome, Response,
    WaitOutcome,
};
use serde_json::json;
use std::sync::Arc;

/// One claim operation, as received from a client.
pub enum ClaimCall {
    Claim(ClaimRequest),
    Share(ClaimRequest),
    Wait(ClaimRequest),
    Release(ClaimRequest),
    List(Option<String>),
}

/// Runs a claim operation on the blocking pool, as change requests do:
/// SQLite I/O must not stall the async workers.
pub async fn handle(state: &Arc<ServerState>, call: ClaimCall) -> Outcome {
    let state = Arc::clone(state);
    finish(tokio::task::spawn_blocking(move || run(&state, call)).await)
}

/// Abandons a change, then releases its claims and wakes their waiters
/// (R6). The release is a second transaction; if the server stops between
/// the two, the abandoned change's records are already inert because only
/// open and ready changes hold claims, and the next release clears them.
pub async fn abandon(state: &Arc<ServerState>, change: String) -> Outcome {
    let state = Arc::clone(state);
    let work = move || {
        let record = state.changes.abandon(&change)?;
        let released = state
            .changes
            .with_conn(|conn| store::release_all(conn, record.change.id))?;
        publish_release(&state, &released, true);
        Ok(Response::Change(record))
    };
    finish(tokio::task::spawn_blocking(work).await)
}

/// Releases a landed change's claims and wakes their waiters. Landing's
/// `ChangeBook::release_claims` calls this; landing itself then emits the
/// landed change's own `released` event, so only the wakes are published here.
pub fn release_landed(state: &ServerState, id: ChangeId) -> Result<ReleaseOutcome, ChangeError> {
    let released = state
        .changes
        .with_conn(|conn| store::release_all(conn, id))?;
    publish_release(state, &released, false);
    Ok(released)
}

fn run(state: &ServerState, call: ClaimCall) -> Result<Response, ChangeError> {
    let changes = &state.changes;
    Ok(match call {
        ClaimCall::Claim(req) => {
            let outcome = changes.with_conn(|c| store::claim(c, &req.change, &req.paths))?;
            if let ClaimOutcome::Granted { change, paths } = &outcome {
                let detail = json!({ "paths": paths });
                state
                    .events
                    .publish(EventKind::Claim, Some(change.to_string()), detail);
            }
            Response::Claimed(outcome)
        }
        ClaimCall::Share(req) => {
            Response::Shared(changes.with_conn(|c| store::share(c, &req.change, &req.paths))?)
        }
        ClaimCall::Wait(req) => {
            // Read the cursor before the wait is stored. Any release that can
            // wake this wait commits after it, and publishes after that, so
            // its event is always after this cursor.
            let cursor = state.events.last_seq();
            let (change, waiting_on, free) =
                changes.with_conn(|c| store::wait(c, &req.change, &req.paths))?;
            Response::Waiting(WaitOutcome {
                change,
                waiting_on,
                free,
                cursor,
            })
        }
        ClaimCall::Release(req) => {
            let released = changes.with_conn(|c| store::release(c, &req.change, &req.paths))?;
            publish_release(state, &released, true);
            Response::Released(released)
        }
        ClaimCall::List(change) => {
            Response::PathClaims(changes.with_conn(|c| store::list(c, change.as_deref()))?)
        }
    })
}

/// One `released` event for the releasing change, then one for each woken
/// waiter, addressed to the waiter's change so its subscriber can wake.
fn publish_release(state: &ServerState, released: &ReleaseOutcome, include_releaser: bool) {
    let releaser = released.change.to_string();
    if include_releaser && !released.released.is_empty() {
        let paths: Vec<&str> = released.released.iter().map(|r| r.path.as_str()).collect();
        let detail = json!({ "paths": paths, "woken": released.woken });
        state
            .events
            .publish(EventKind::Released, Some(releaser.clone()), detail);
    }
    for woken in &released.woken {
        let detail = json!({ "path": woken.path, "released_by": releaser, "wake": true });
        state
            .events
            .publish(EventKind::Released, Some(woken.change.to_string()), detail);
    }
}

fn finish(joined: Result<Result<Response, ChangeError>, tokio::task::JoinError>) -> Outcome {
    match joined {
        Ok(Ok(response)) => Outcome::Ok(response),
        Ok(Err(err)) => Outcome::Error(ErrorBody::new(
            crate::rpc::change_error_code(&err),
            err.to_string(),
        )),
        Err(join) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, join.to_string())),
    }
}
