//! Path claims, shares and waits (SPEC R6). They are also AIRC's file claims
//! (R13): one record per path, held in the metadata store with the changes.
//!
//! The first-claim-wins rule is pure logic in `rosen_core::claim`; [`store`]
//! runs it inside one SQLite transaction per operation. Events are published
//! only after that transaction commits, so a subscriber never sees a claim or
//! a wake that a rollback undid.

mod delivery;
pub(crate) mod store;

pub use delivery::recover;
pub use store::ensure_schema;

/// Every claim, share and wait record of an open or ready change, oldest
/// first. Overlap detection reads them (R6).
pub fn live_records(
    conn: &mut rusqlite::Connection,
) -> Result<Vec<rosen_protocol::PathClaim>, ChangeError> {
    store::list(conn, None)
}

use crate::changes::ChangeError;
use crate::ServerState;
use rosen_core::ChangeId;
use rosen_protocol::{
    ClaimOutcome, ClaimRequest, ErrorBody, ErrorCode, Outcome, ReleaseOutcome, Response,
    WaitOutcome,
};
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

/// Abandons a change and releases its claims in one transaction (R6).
pub async fn abandon(state: &Arc<ServerState>, change: String) -> Outcome {
    let state = Arc::clone(state);
    let work = move || {
        // The abandon staged its `updated` event in its own transaction.
        let (record, _) = state.changes.abandon_releasing(&change)?;
        state.events.deliver_staged();
        recover(&state)?;
        Ok(Response::Change(record))
    };
    finish(tokio::task::spawn_blocking(work).await)
}

/// Releases a landed change's claims and wakes their waiters. Landing's
/// `ChangeBook::release_claims` calls this. Notifications are durable so
/// replay can finish delivery even after the claim records are gone.
pub fn release_landed(state: &ServerState, id: ChangeId) -> Result<ReleaseOutcome, ChangeError> {
    let released = state
        .changes
        .with_conn(|conn| store::release_all(conn, id))?;
    recover(state)?;
    Ok(released)
}

fn run(state: &ServerState, call: ClaimCall) -> Result<Response, ChangeError> {
    let changes = &state.changes;
    Ok(match call {
        ClaimCall::Claim(req) => {
            let outcome = changes.with_conn(|c| store::claim(c, &req.change, &req.paths))?;
            // A grant staged its `claim` event in the claim's transaction.
            if matches!(outcome, ClaimOutcome::Granted { .. }) {
                state.events.deliver_staged();
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
            recover(state)?;
            Response::Released(released)
        }
        ClaimCall::List(change) => {
            Response::PathClaims(changes.with_conn(|c| store::list(c, change.as_deref()))?)
        }
    })
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

/// Retained records of one change, including a just-landed subject whose
/// claims still select post-land rebase targets before release.
pub fn records_for(
    conn: &mut rusqlite::Connection,
    id: rosen_core::ChangeId,
) -> Result<Vec<rosen_protocol::PathClaim>, ChangeError> {
    store::list(conn, Some(&id.to_string()))
}
