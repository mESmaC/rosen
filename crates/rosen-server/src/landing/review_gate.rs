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

    /// Every change is `normal` until the server tiers changes from their
    /// diff: a normal change needs a receipt, so this never admits more.
    fn tier(&self, _: &Change) -> Result<Tier, String> {
        Ok(Tier::Normal)
    }
}

fn approvers(
    conn: &rusqlite::Connection,
    change: &Change,
) -> Result<Option<(CommitId, Vec<AgentId>)>, ReviewError> {
    if approvals::current(conn, change.id, change.head)?.is_none() {
        return Ok(None);
    }
    let mut reviewers: Vec<AgentId> = Vec::new();
    for approval in approvals::list(conn, change.id)? {
        if approval.valid && !reviewers.contains(&approval.reviewer) {
            reviewers.push(approval.reviewer);
        }
    }
    Ok(Some((change.head, reviewers)))
}
