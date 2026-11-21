//! Collapse link verification and feed reset (SPEC R1, R9, R10, #435, #445).

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{CollapseCheckReport, CollapseResetReport, Request, Response};

impl Client {
    /// Verifies the server's Collapse link end to end without making any changes.
    pub async fn collapse_check(&self) -> Result<CollapseCheckReport, Error> {
        match self.request(Request::CollapseCheck).await? {
            Response::CollapseCheck(report) => Ok(*report),
            other => Err(unexpected(other)),
        }
    }

    /// Asks the server to reset this repository's Collapse feed tables and
    /// resume sending with a fresh feed ID (`rosen collapse link --reset`).
    pub async fn collapse_reset(&self) -> Result<CollapseResetReport, Error> {
        match self.request(Request::CollapseReset).await? {
            Response::CollapseReset(report) => Ok(report),
            other => Err(unexpected(other)),
        }
    }
}
