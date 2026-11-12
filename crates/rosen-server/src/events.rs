//! The server's event log and fan-out to subscribed connections (SPEC R9).
//!
//! Every event gets a sequence number, which is the cursor a client resumes from
//! after reconnecting. Subscribers read the log from their cursor, then sleep on
//! a watch channel until a newer sequence number is published, so delivery never
//! depends on a bounded channel keeping up.

use rosen_protocol::{Event, EventKind};
use std::sync::Mutex;
use tokio::sync::watch;

pub struct EventLog {
    // v1 keeps the log in memory for the daemon's lifetime; persisting it to the
    // metadata store (so cursors survive a daemon restart) belongs to R9.
    events: Mutex<Vec<Event>>,
    latest: watch::Sender<u64>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            latest: watch::Sender::new(0),
        }
    }
}

impl EventLog {
    /// Appends an event and wakes every subscriber. Returns its sequence number.
    pub fn publish(
        &self,
        kind: EventKind,
        change: Option<String>,
        detail: serde_json::Value,
    ) -> u64 {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let seq = events.len() as u64 + 1;
        events.push(Event {
            seq,
            kind,
            change,
            detail,
        });
        // Publish while holding the lock so subscribers see sequence numbers in order.
        self.latest.send_replace(seq);
        seq
    }

    /// Every event with a sequence number greater than `cursor`, oldest first.
    pub fn after(&self, cursor: u64) -> Vec<Event> {
        let events = self
            .events
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let start = usize::try_from(cursor)
            .unwrap_or(usize::MAX)
            .min(events.len());
        events[start..].to_vec()
    }

    /// The newest sequence number, 0 when the log is empty.
    pub fn last_seq(&self) -> u64 {
        *self.latest.borrow()
    }

    /// A receiver that changes whenever a new event is published.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.latest.subscribe()
    }
}
