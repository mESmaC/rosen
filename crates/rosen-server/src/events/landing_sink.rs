//! Routes landing's events (SPEC R8 step 4) into the event log.

use crate::landing::{EventSink, LandingEvent};
use crate::ServerState;
use rosen_protocol::EventKind;
use serde_json::json;
use std::sync::Arc;

/// The [`EventSink`] the server gives landing. Each event is published about
/// its change, so it reaches the change's owner, the changes stacked on it
/// and the holders of overlapping claims (R9).
pub struct LandingEvents(pub Arc<ServerState>);

impl EventSink for LandingEvents {
    fn emit(&self, event: LandingEvent) {
        // Conflict routing staged this event with the conflicts it stored
        // (`conflict_routes::record`), so a stop after that commit cannot
        // lose it (R9). Only its delivery is left.
        if matches!(event, LandingEvent::Conflict { .. }) {
            self.0.events.deliver_staged();
            return;
        }
        let (kind, change, detail) = parts(event);
        self.0
            .events
            .publish(kind, Some(change.to_string()), detail);
    }

    fn emit_once(&self, event: LandingEvent) {
        let (kind, change, detail) = parts(event);
        let change = change.to_string();
        if !self.0.events.holds(kind, &change, &detail) {
            self.0.events.publish(kind, Some(change), detail);
        }
    }
}

fn parts(event: LandingEvent) -> (EventKind, crate::landing::ChangeId, serde_json::Value) {
    match event {
        LandingEvent::Landed { change, commit } => (
            EventKind::Landed,
            change,
            json!({ "commit": commit.to_hex() }),
        ),
        LandingEvent::Rebased { change, onto } => {
            (EventKind::Rebased, change, json!({ "onto": onto.to_hex() }))
        }
        LandingEvent::Conflict { change, paths } => {
            (EventKind::Conflict, change, json!({ "paths": paths }))
        }
        // Waiters woken by the release get their own `released` events
        // from the claim store (`claims::release_landed`).
        // The commit tells two lands of one change apart (a reland after
        // a revert), so a replay of the second is not taken for the first.
        LandingEvent::Released { change, commit } => (
            EventKind::Released,
            change,
            json!({ "reason": "landed", "commit": commit.to_hex() }),
        ),
    }
}
