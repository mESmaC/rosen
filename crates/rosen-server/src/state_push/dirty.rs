//! Which documents the events after the cursor make dirty (R9).
//!
//! An event naming a change makes that change dirty, and a claim makes the
//! changes it overlaps dirty too. `landed`, `review`, `green` and
//! `reverted` make coverage dirty, and `landed` the configuration, which
//! is only sent when its fingerprint shows `rosen.toml` changed.
//!
//! Post-land events and mirror reports are lines the feed sends (#406).
//! A channel `joined`, `left` or `mode` line makes presence dirty; other
//! channel lines are sent only when the project opts in, and never ledger
//! entries. The feed's and the ledger push's own reports never wake it.

use std::collections::BTreeSet;
use std::time::Instant;

use rosen_core::ChangeId;
use rosen_protocol::{Event, EventKind};
use serde_json::Value;

use crate::events::EventLog;

/// How many events one read of the log takes.
const PAGE: u64 = 2000;

/// Documents waiting to be sent, and since when.
#[derive(Debug, Default)]
pub struct Pending {
    pub changes: BTreeSet<ChangeId>,
    pub coverage: bool,
    pub config: bool,
    /// Event lines, or channel lines when they are sent, wait in the log.
    pub log: bool,
    pub machines: bool,
    pub presence: bool,
    /// The relay's allowed kinds (R14), which may have changed.
    pub relay: bool,
    /// The presence document goes with this pass even unchanged, if any
    /// member is joined and anything else goes.
    pub refresh_presence: bool,
    /// Every live document, whatever was delivered before: set on start
    /// and after a failure, so the endpoint converges after any loss.
    pub everything: bool,
    /// The event log has been read through this sequence.
    pub scanned_to: u64,
    /// When the oldest pending event other than a snapshot arrived.
    pub urgent_since: Option<Instant>,
    /// When the oldest pending snapshot arrived.
    pub snapshot_since: Option<Instant>,
    /// When the machine list or presence last changed: they go after a
    /// quiet spell, or with whatever goes sooner.
    pub quiet_since: Option<Instant>,
}

impl Pending {
    /// Everything live, read from `cursor` on.
    pub fn everything(cursor: u64) -> Self {
        Self {
            coverage: true,
            config: true,
            log: true,
            machines: true,
            presence: true,
            relay: true,
            everything: true,
            scanned_to: cursor,
            urgent_since: Some(Instant::now()),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
            && !self.coverage
            && !self.config
            && !self.log
            && !self.machines
            && !self.presence
            && !self.relay
            && !self.everything
    }

    /// Marks the machine list changed.
    pub fn machines_changed(&mut self) {
        self.machines = true;
        // Revoking the `collapse` machine turns the relay off.
        self.relay = true;
        self.quiet_since = Some(Instant::now());
    }

    /// Reads events after `scanned_to` and marks what they make dirty;
    /// channel lines only count as news when `channel` sends them. Reads
    /// SQLite: call it on the blocking pool.
    pub fn scan(
        &mut self,
        events: &EventLog,
        channel: bool,
    ) -> Result<(), crate::events::EventError> {
        loop {
            let page = events.page(self.scanned_to, None, PAGE)?;
            for event in &page.events {
                self.mark(event, channel);
            }
            if page.scanned_to <= self.scanned_to {
                return Ok(());
            }
            self.scanned_to = page.scanned_to;
        }
    }

    fn mark(&mut self, event: &Event, channel: bool) {
        match event.kind {
            EventKind::Channel => return self.mark_line(&event.detail, channel),
            EventKind::LedgerPush | EventKind::StatePush => return,
            EventKind::Relay => return self.mark_relay(&event.detail),
            _ => {}
        }
        let mut marked = false;
        if is_sent_line(event.kind) {
            self.log = true;
            marked = true;
        }
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

    /// A relay report: a new setting changes what Collapse is told, and a
    /// relayed revoke (#409) the machine list. The owner who revoked from
    /// Collapse waits on that list, so it goes without the quiet spell.
    fn mark_relay(&mut self, detail: &Value) {
        let status = detail.get("status").and_then(Value::as_str);
        let kind = detail.get("kind").and_then(Value::as_str);
        match (status, kind) {
            (Some("setting"), _) => self.relay = true,
            (Some("done"), Some("machine_revoke")) => self.machines = true,
            _ => return,
        }
        self.urgent_since.get_or_insert_with(Instant::now);
    }

    /// A channel line: presence when membership or a mode changed, and the
    /// line itself when the project sends them.
    fn mark_line(&mut self, detail: &Value, channel: bool) {
        let line = detail.get("line").and_then(Value::as_str);
        if matches!(line, Some("joined" | "left" | "mode")) {
            self.presence = true;
            self.quiet_since = Some(Instant::now());
        }
        if channel && line != Some("ledger") {
            self.log = true;
            self.urgent_since.get_or_insert_with(Instant::now);
        }
    }
}

/// Whether the feed sends events of `kind` as lines; the same kinds
/// [`super::log_lines::SENT_KINDS`] reads from the log.
fn is_sent_line(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Conflict
            | EventKind::Built
            | EventKind::Green
            | EventKind::Reverted
            | EventKind::RevertRefused
            | EventKind::PostLandFailure
            | EventKind::Bisect
            | EventKind::Mirror
    )
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
