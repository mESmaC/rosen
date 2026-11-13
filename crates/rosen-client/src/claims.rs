//! Typed calls for path claims, shares, waits and releases (SPEC R6).

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{
    ClaimFilter, ClaimOutcome, ClaimRequest, Event, EventKind, PathClaim, ReleaseOutcome, Request,
    Response, ShareOutcome, WaitOutcome,
};
use std::collections::BTreeSet;

impl Client {
    /// Claims every path for the change, or none of them. A refusal is
    /// [`ClaimOutcome::Refused`] naming the holders, not an error.
    pub async fn claim_paths(&self, request: ClaimRequest) -> Result<ClaimOutcome, Error> {
        match self.request(Request::ClaimPaths(request)).await? {
            Response::Claimed(outcome) => Ok(outcome),
            other => Err(unexpected(other)),
        }
    }

    pub async fn share_paths(&self, request: ClaimRequest) -> Result<ShareOutcome, Error> {
        match self.request(Request::SharePaths(request)).await? {
            Response::Shared(outcome) => Ok(outcome),
            other => Err(unexpected(other)),
        }
    }

    /// Records the wait and returns at once; see [`Client::wait_until_free`]
    /// to block until it ends.
    pub async fn wait_paths(&self, request: ClaimRequest) -> Result<WaitOutcome, Error> {
        match self.request(Request::WaitPaths(request)).await? {
            Response::Waiting(outcome) => Ok(outcome),
            other => Err(unexpected(other)),
        }
    }

    /// Records the wait, then blocks until every blocked path is woken by a
    /// `released` event, or the change's own release drops the wait.
    pub async fn wait_until_free(&self, request: ClaimRequest) -> Result<WaitOutcome, Error> {
        let outcome = self.wait_paths(request).await?;
        let mut pending: BTreeSet<String> = outcome
            .waiting_on
            .iter()
            .map(|holder| holder.requested.clone())
            .collect();
        if pending.is_empty() {
            return Ok(outcome);
        }
        // Subscribing from the cursor read before the wait was stored means a
        // release that raced this call is replayed, never missed.
        let mut events = self.subscribe(Some(outcome.cursor)).await?;
        let waiter = outcome.change.to_string();
        while let Some(event) = events.recv().await {
            for path in ended_waits(&event, &waiter) {
                pending.remove(&path);
            }
            if pending.is_empty() {
                return Ok(outcome);
            }
        }
        Err(Error::Disconnected)
    }

    /// Drops the change's records on `paths`, or every record when empty.
    pub async fn release_paths(&self, request: ClaimRequest) -> Result<ReleaseOutcome, Error> {
        match self.request(Request::ReleasePaths(request)).await? {
            Response::Released(outcome) => Ok(outcome),
            other => Err(unexpected(other)),
        }
    }

    pub async fn claim_list(&self, filter: ClaimFilter) -> Result<Vec<PathClaim>, Error> {
        match self.request(Request::ClaimList(filter)).await? {
            Response::PathClaims(records) => Ok(records),
            other => Err(unexpected(other)),
        }
    }
}

/// The waited paths a `released` event for `waiter` ends: a wake names one
/// path, and the waiter's own release lists every path it dropped.
fn ended_waits(event: &Event, waiter: &str) -> Vec<String> {
    if event.kind != EventKind::Released || event.change.as_deref() != Some(waiter) {
        return Vec::new();
    }
    let detail = &event.detail;
    let single = detail
        .get("path")
        .and_then(|p| p.as_str())
        .map(str::to_owned);
    let listed = detail
        .get("paths")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_owned));
    single.into_iter().chain(listed).collect()
}
