//! The server's review gate (SPEC R7): a land needs an approval for the
//! change's current head, read from the stored review approvals.
//!
//! This read decides the land. The ledger checks the approval again inside
//! the compare-and-swap transaction, so an author snapshot stored between
//! the two still stops the land.

use std::sync::{Arc, Weak};

use rosen_core::{AgentId, Change};

use super::service::ReviewGate;
use super::{CommitId, Tier};
use crate::reviews::{approvals, ReviewError};
use crate::ServerState;

/// Approvals from the review store. Holds the server weakly because the
/// server holds the landing service that holds this gate.
pub struct StoredReviews(pub Weak<ServerState>);

impl StoredReviews {
    fn state(&self) -> Result<Arc<ServerState>, String> {
        self.0
            .upgrade()
            .ok_or_else(|| "the server is stopping".to_owned())
    }
}

impl ReviewGate for StoredReviews {
    /// The change's current head and every reviewer whose approval still
    /// holds there, in approval order. An author snapshot voids approvals; a
    /// server rebase keeps them, so a surviving approval may name an older
    /// head yet still cover the rebased one.
    fn approval(&self, change: &Change) -> Result<Option<(CommitId, Vec<AgentId>)>, String> {
        let state = self.state()?;
        let read = |conn: &mut rusqlite::Connection| Ok(approvers(conn, change));
        state
            .changes
            .with_conn(read)
            .map_err(|err| err.to_string())?
            .map_err(|err| err.to_string())
    }

    /// Tier the actual edits under the current base policy.
    fn tier(&self, change: &Change) -> Result<Tier, String> {
        let state = self.state()?;
        crate::reviews::tiers::for_change(&state, change)
    }
}

fn approvers(
    conn: &rusqlite::Connection,
    change: &Change,
) -> Result<Option<(CommitId, Vec<AgentId>)>, ReviewError> {
    if approvals::current(conn, change.id, change.head)?.is_none() {
        return Ok(None);
    }
    // Each named as `<nick>@<machine>` when its machine is known (R14), for
    // the landed commit's `Rosen-Approved-By` trailers.
    let mut reviewers: Vec<AgentId> = Vec::new();
    for approval in approvals::list(conn, change.id)? {
        if !approval.valid {
            continue;
        }
        let reviewer = match &approval.machine {
            Some(machine) => approval.reviewer.on_machine(machine)?,
            None => approval.reviewer,
        };
        if !reviewers.contains(&reviewer) {
            reviewers.push(reviewer);
        }
    }
    Ok(Some((change.head, reviewers)))
}
