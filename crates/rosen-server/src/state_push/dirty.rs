//! Which documents the events after the cursor make dirty (R9).
//!
//! An event naming a change makes that change dirty, and a claim makes the
//! changes it overlaps dirty too. `landed`, `review`, `green` and
//! `reverted` make coverage dirty, and `landed` the configuration, which
//! is only sent when its fingerprint shows `rosen.toml` changed. Channel
//! lines, mirror and push reports never make anything dirty: the feed
//! sends no messages, and its own reports must not wake it.

use std::collections::BTreeSet;
use std::time::Instant;

use rosen_core::ChangeId;
use rosen_protocol::{Event, EventKind};

use crate::events::EventLog;

/// How many events one read of the log takes.
const PAGE: u64 = 2000;

/// Documents waiting to be sent, and since when.
#[derive(Debug, Default)]
pub struct Pending {
    pub changes: BTreeSet<ChangeId>,
    pub coverage: bool,
    pub config: bool,
    /// Every live document, whatever was delivered before: set on start
    /// and after a failure, so the endpoint converges after any loss.
    pub everything: bool,
    /// The event log has been read through this sequence.
    pub scanned_to: u64,
    /// When the oldest pending event other than a snapshot arrived.
    pub urgent_since: Option<Instant>,
    /// When the oldest pending snapshot arrived.
    pub snapshot_since: Option<Instant>,
}

impl Pending {
    /// Everything live, read from `cursor` on.
    pub fn everything(cursor: u64) -> Self {
        Self {
            coverage: true,
            config: true,
            everything: true,
            scanned_to: cursor,
            urgent_since: Some(Instant::now()),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && !self.coverage && !self.config && !self.everything
    }

    /// Reads events after `scanned_to` and marks what they make dirty.
    /// Reads SQLite: call it on the blocking pool.
    pub fn scan(&mut self, events: &EventLog) -> Result<(), crate::events::EventError> {
        loop {
            let page = events.page(self.scanned_to, None, PAGE)?;
            for event in &page.events {
                self.mark(event);
            }
            if page.scanned_to <= self.scanned_to {
                return Ok(());
            }
            self.scanned_to = page.scanned_to;
        }
    }

    fn mark(&mut self, event: &Event) {
        if matches!(
            event.kind,
            EventKind::Channel | EventKind::Mirror | EventKind::LedgerPush | EventKind::StatePush
        ) {
            return;
        }
        let mut marked = false;
        if let Some(id) = event.change.as_deref().and_then(|id| id.parse().ok()) {
            marked |= self.changes.insert(id);
        }
        if event.kind == EventKind::Claim {
            for id in overlapping(&event.detail) {
                marked |= self.changes.insert(id);
            }
        }
        if matches!(
            event.kind,
            EventKind::Landed | EventKind::Review | EventKind::Green | EventKind::Reverted
        ) {
            self.coverage = true;
            marked = true;
        }
        if event.kind == EventKind::Landed {
            self.config = true;
        }
        if !marked {
            return;
        }
        let since = if event.kind == EventKind::Snapshot {
            &mut self.snapshot_since
        } else {
            &mut self.urgent_since
        };
        since.get_or_insert_with(Instant::now);
    }
}

/// The changes a `claim` event names as overlapping it.
fn overlapping(detail: &serde_json::Value) -> impl Iterator<Item = ChangeId> + '_ {
    detail
        .get("overlaps")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|overlap| overlap.get("change")?.as_str()?.parse().ok())
}
