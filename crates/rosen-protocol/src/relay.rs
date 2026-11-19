//! The Collapse action relay's operator setting (R14 "Collapse action
//! relay"): which owner requests the server fetches from its Collapse
//! endpoint and runs.

use serde::{Deserialize, Serialize};

/// The reserved machine relayed actions are recorded under, so approvals
/// and marks read `<account>@collapse`. Revoking it turns the relay off.
pub const COLLAPSE_MACHINE: &str = "collapse";

/// An action kind the operator can allow. Claims, messages, receipts,
/// abandon, rebase, conflict resolution, machine commands and imports are
/// never relayed, so they have no kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayKind {
    /// `review approve` of a change's current head (R7).
    Approve,
    /// `land` with every gate (R8).
    Land,
    /// `review mark` of a landed change (R7 coverage).
    Mark,
}

impl RelayKind {
    pub const ALL: [RelayKind; 3] = [RelayKind::Approve, RelayKind::Land, RelayKind::Mark];

    pub fn as_str(self) -> &'static str {
        match self {
            RelayKind::Approve => "approve",
            RelayKind::Land => "land",
            RelayKind::Mark => "mark",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// The relay as the operator set it, and whether it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySetting {
    /// The kinds the operator allowed, sorted; empty when it is off.
    pub allow: Vec<RelayKind>,
    /// Whether `rosen machine revoke collapse` turned it off. Allowing any
    /// kind again re-enrolls the machine.
    #[serde(default)]
    pub revoked: bool,
}

impl RelaySetting {
    /// Whether the server fetches and runs actions at all.
    pub fn active(&self) -> bool {
        !self.allow.is_empty() && !self.revoked
    }

    /// The kinds Collapse may offer: none while the relay is off.
    pub fn offered(&self) -> Vec<RelayKind> {
        if self.revoked {
            Vec::new()
        } else {
            self.allow.clone()
        }
    }
}
