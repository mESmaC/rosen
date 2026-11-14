//! Channel producers use the Rosen log, wakeups and acknowledged cursors (R13).

use super::{pending, store, EventError, EventLog};
use rosen_core::AgentId;
use rosen_protocol::EventKind;
use rusqlite::{Connection, Transaction, TransactionBehavior};

/// One channel line to append with the mutation that caused it.
pub struct ChannelLine {
    pub detail: serde_json::Value,
    /// The agents who must act on it. Empty reaches only the project view.
    pub recipients: Vec<AgentId>,
}

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
        self.enqueue(EventKind::Channel, None, detail, |_| Some(audience), None)
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
        let line = || ChannelLine {
            detail: detail.clone(),
            recipients: recipients.to_vec(),
        };
        let (value, seq) = self.commit_lines(|tx| {
            update(tx)
                .map(|value| (value, vec![line()]))
                .map_err(EventError::from)
        })?;
        Ok((value, seq.unwrap_or(0)))
    }

    /// [`EventLog::commit_channel`] for a mutation that decides its own lines
    /// from what it read and wrote: none, one or several, appended in order.
    ///
    /// The transaction is IMMEDIATE: it holds the write lock from its first
    /// read, so the read and the write it decides (such as first claim wins)
    /// cannot interleave with another connection's write.
    pub fn commit_channel_lines<T, E: From<EventError>>(
        &self,
        update: impl FnOnce(&Transaction<'_>) -> Result<(T, Vec<ChannelLine>), E>,
    ) -> Result<T, E> {
        self.commit_lines(update).map(|(value, _)| value)
    }

    /// Commits `update` and its lines; returns the last line's sequence.
    fn commit_lines<T, E: From<EventError>>(
        &self,
        update: impl FnOnce(&Transaction<'_>) -> Result<(T, Vec<ChannelLine>), E>,
    ) -> Result<(T, Option<u64>), E> {
        self.inner.with_slot(|slot| {
            let conn = self
                .inner
                .connection(&mut slot.conn, true)?
                .expect("creating the event database opens a connection");
            pending::flush(&self.inner, conn, &mut slot.queue)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(EventError::from)?;
            let (value, lines) = update(&tx)?;
            let mut last = None;
            for line in &lines {
                let audience = line
                    .recipients
                    .iter()
                    .map(|agent| agent.as_str().to_owned())
                    .collect();
                let kind = EventKind::Channel;
                let seq = store::append_in(&tx, kind, None, &line.detail, Some(&audience))
                    .map_err(EventError::from)?;
                last = Some(seq);
            }
            tx.commit().map_err(EventError::from)?;
            // Readers wake only after both state and event have committed.
            if let Some(seq) = last {
                self.inner.latest.send_replace(seq);
            }
            Ok((value, last))
        })
    }

    /// Reads channel state on the log's connection, so a read never sees a
    /// mutation whose line is not yet in the log.
    pub fn read_channel<T, E: From<EventError>>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, E>,
    ) -> Result<T, E> {
        self.inner.with_slot(|slot| {
            let conn = self
                .inner
                .connection(&mut slot.conn, true)?
                .expect("creating the event database opens a connection");
            read(conn)
        })
    }
}
