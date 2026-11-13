//! Typed call for landing a change (SPEC R8 step 2).

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{LandAnswer, Request, Response};

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
