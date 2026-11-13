//! Typed access to live workspace edit tracking (SPEC R6).
use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{EditRegions, Request, Response};

impl Client {
    /// Reads paths and parent-line regions for the latest durable head.
    /// Local writes appear after their automatic snapshot reaches the server;
    /// server workspace writes appear as soon as their request completes.
    pub async fn edit_regions(&self, change: &str) -> Result<EditRegions, Error> {
        match self
            .request(Request::EditRegions {
                change: change.into(),
            })
            .await?
        {
            Response::EditRegions(view) => Ok(view),
            other => Err(unexpected(other)),
        }
    }
}
