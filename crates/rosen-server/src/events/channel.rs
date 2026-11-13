//! Channel producers use the Rosen log, wakeups and acknowledged cursors (R13).

use super::{pending, store, EventError, EventLog};
use rosen_core::AgentId;
use rosen_protocol::EventKind;

impl EventLog {
    /// Appends a channel line with the agents who must act on it. The channel
    /// authority resolves nicks to agent identities before publishing, so
    /// delivery works across machines without local presence or claim state.
    ///
    /// The audience is fixed here, including across write retries. An empty
    /// audience sends only to the project view; it never becomes a broadcast.
    /// Read and acknowledge these events with the usual R9 subscription API.
    pub fn publish_channel(&self, detail: serde_json::Value, recipients: &[AgentId]) -> u64 {
        let audience = recipients
            .iter()
            .map(|agent| agent.as_str().to_owned())
            .collect();
        self.enqueue(EventKind::Channel, None, detail, |_| Some(audience))
    }

    /// Commits a channel mutation and its event together. A failure rolls back
    /// both, so a successful tool call cannot lose its notification on restart.
    /// `update` runs on the log's metadata connection; it must not call the log
    /// recursively. Initialize channel tables before using them here.
    ///
    /// Older queued Rosen events are flushed first. If that fails, no channel
    /// state changes, preserving the single publication order across producers.
    pub fn commit_channel<T>(
        &self,
        detail: &serde_json::Value,
        recipients: &[AgentId],
        update: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<(T, u64), EventError> {
        self.inner.with_slot(|slot| {
            let conn = self
                .inner
                .connection(&mut slot.conn, true)?
                .expect("creating the event database opens a connection");
            pending::flush(&self.inner, conn, &mut slot.queue)?;
            let tx = conn.transaction()?;
            let value = update(&tx)?;
            let audience = recipients
                .iter()
                .map(|agent| agent.as_str().to_owned())
                .collect();
            let seq = store::append_in(&tx, EventKind::Channel, None, detail, Some(&audience))?;
            tx.commit()?;
            // Readers wake only after both state and event have committed.
            self.inner.latest.send_replace(seq);
            Ok((value, seq))
        })
    }
}
