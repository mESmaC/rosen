//! Watching first-claim-wins (R6, §5 "no two agents ever hold the same
//! path claim, on any machine").
//!
//! Two views catch a double holder. Within one machine the harness knows
//! exactly when each of its agents holds the hot path. Across machines,
//! each grant is followed by a read of the server's claim list, which must
//! show the grantee as the path's only claimant.

use rosen_client::protocol::{ClaimKind, PathClaim};
use rosen_client::ChangeId;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct HotClaims {
    holders: Mutex<HashMap<String, Holder>>,
}

struct Holder {
    agent: usize,
    change: ChangeId,
    /// The holder's land request is in flight. The server releases claims
    /// in the land's own transaction, before the answer arrives, so another
    /// grant during this window is legitimate.
    landing: bool,
}

impl HotClaims {
    /// Records a grant; returns a violation when another of this machine's
    /// agents still held the path and was not landing.
    pub fn granted(&self, path: &str, agent: usize, change: ChangeId) -> Option<String> {
        let mut holders = self.holders.lock().expect("hot claims lock");
        let clash = holders
            .get(path)
            .filter(|held| held.agent != agent && !held.landing)
            .map(|held| {
                format!(
                    "{path} granted to {change} while {} still held it",
                    held.change
                )
            });
        let holder = Holder {
            agent,
            change,
            landing: false,
        };
        holders.insert(path.to_owned(), holder);
        clash
    }

    pub fn landing(&self, change: ChangeId) {
        let mut holders = self.holders.lock().expect("hot claims lock");
        for held in holders.values_mut().filter(|held| held.change == change) {
            held.landing = true;
        }
    }

    /// The change landed or was abandoned, so the server released it.
    pub fn released(&self, change: ChangeId) {
        let mut holders = self.holders.lock().expect("hot claims lock");
        holders.retain(|_, held| held.change != change);
    }

    /// A violation when the server lists more than one claim on `path`.
    pub fn server_clash(path: &str, claims: &[PathClaim]) -> Option<String> {
        let holders: Vec<String> = claims
            .iter()
            .filter(|claim| claim.kind == ClaimKind::Claim && claim.path == path)
            .map(|claim| format!("{} ({})", claim.change, claim.owner))
            .collect();
        (holders.len() > 1).then(|| format!("server lists {path} claimed by {holders:?}"))
    }
}
