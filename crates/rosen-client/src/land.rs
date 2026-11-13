//! Typed call for landing a change (SPEC R8 step 2).

use std::time::Duration;

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{Endpoint, LandAnswer, Request, Response};

/// How many times [`land_reconnecting`] asks before giving up on a server
/// that keeps dropping the connection, and its first wait between asks.
const LAND_ASKS: u32 = 8;
const FIRST_WAIT: Duration = Duration::from_millis(200);

impl Client {
    /// Asks the server to land `change`, a change ID or unique prefix. The
    /// answer says whether it landed or names what refused it; a refusal is
    /// an answer, not an error. The server retries a lost compare-and-swap
    /// itself while the receipt still counts.
    pub async fn land(&self, change: &str) -> Result<LandAnswer, Error> {
        let request = Request::Land {
            change: change.to_owned(),
        };
        match self.request(request).await? {
            Response::Land(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }
}

/// Lands `change` through a server that may crash or restart mid-request.
///
/// A dropped connection leaves the caller not knowing whether the land
/// committed. Asking again is safe: the server lands a change at most once
/// and answers a repeat with `already_landed` and the landed commit, so the
/// answer is never lost and the land is never applied twice. Each ask
/// connects afresh, starting the server when none is running.
pub async fn land_reconnecting(endpoint: &Endpoint, change: &str) -> Result<LandAnswer, Error> {
    let mut wait = FIRST_WAIT;
    let mut ask = 1;
    loop {
        let answer = match crate::connect_or_start(endpoint).await {
            Ok(client) => client.land(change).await,
            Err(error) => Err(error),
        };
        match answer {
            Err(Error::Disconnected | Error::Io(_)) if ask < LAND_ASKS => {
                tokio::time::sleep(wait).await;
                wait *= 2;
                ask += 1;
            }
            settled => return settled,
        }
    }
}
