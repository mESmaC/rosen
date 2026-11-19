//! Operator-only Collapse action relay setting (SPEC R14).
use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{RelayKind, RelaySetting, Request, Response};

impl Client {
    /// Sets the action kinds the server fetches from Collapse and runs;
    /// an empty list turns the relay off. Run against the server's local
    /// endpoint: it refuses the setting from any other machine.
    pub async fn relay_set(&self, allow: Vec<RelayKind>) -> Result<RelaySetting, Error> {
        match self.request(Request::RelaySet { allow }).await? {
            Response::Relay(setting) => Ok(setting),
            other => Err(unexpected(other)),
        }
    }
}
