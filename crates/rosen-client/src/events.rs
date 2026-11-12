//! Event cursors (SPEC R9). Streams start with [`Client::subscribe`] and
//! [`Client::subscribe_as`]; this module stores how far an agent has read.

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_core::AgentId;
use rosen_protocol::{Request, Response};

impl Client {
    /// Records that `subscriber` has handled every event up to `seq`, so its
    /// next [`Client::subscribe_as`] with no cursor resumes after it. Ack only
    /// after handling: an event is redelivered until it is acknowledged.
    /// Returns the stored cursor, which never moves backwards.
    pub async fn ack_events(&self, subscriber: AgentId, seq: u64) -> Result<u64, Error> {
        match self.request(Request::AckEvents { subscriber, seq }).await? {
            Response::EventsAcked(stored) => Ok(stored.cursor),
            other => Err(unexpected(other)),
        }
    }
}
