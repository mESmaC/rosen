//! Read-only end-to-end verification of a server's Collapse link (SPEC R1, R10, #435).

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{CollapseCheckReport, Request, Response};

impl Client {
    /// Verifies the server's Collapse link end to end without making any changes.
    pub async fn collapse_check(&self) -> Result<CollapseCheckReport, Error> {
        match self.request(Request::CollapseCheck).await? {
            Response::CollapseCheck(report) => Ok(*report),
            other => Err(unexpected(other)),
        }
    }
}
