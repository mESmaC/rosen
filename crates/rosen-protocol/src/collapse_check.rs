//! `rosen collapse check` report (SPEC R1, R10, #435).

use serde::{Deserialize, Serialize};

/// Outcome of one Collapse link check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollapseCheckStatus {
    Pass,
    Fail,
    Skipped,
}

impl CollapseCheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

/// One probe result in a [`CollapseCheckReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollapseCheckItem {
    /// Stable check identifier, such as `mirror_owner` or `credential_state`.
    pub name: String,
    pub status: CollapseCheckStatus,
    /// Summary of what the probe observed. Never contains tokens or passwords.
    pub detail: String,
    /// Actionable command or edit to fix a failing (or configure a skipped) check.
    #[serde(default)]
    pub hint: Option<String>,
}

/// Read-only verification report for a server's Collapse link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollapseCheckReport {
    /// True when no check failed (every check passed or was skipped).
    pub ok: bool,
    /// Effective mirror remote (with any URL credentials hidden), if configured.
    #[serde(default)]
    pub mirror_remote: Option<String>,
    /// `collapse` root from the base's `rosen.toml`, if configured.
    #[serde(default)]
    pub collapse: Option<String>,
    /// Effective ledger push URL (`ledger_endpoint` or `<collapse>/ledgers`).
    #[serde(default)]
    pub ledger_endpoint: Option<String>,
    /// Whether this server is marked as the mirror owner.
    pub mirror_owner: bool,
    /// Whether HTTPS mirror reads currently have proactive auth cached on this server.
    pub mirror_proactive_auth: bool,
    /// Individual probe results in display order.
    pub checks: Vec<CollapseCheckItem>,
}
