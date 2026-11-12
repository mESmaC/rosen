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
        let (kind, change, detail) = match event {
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
            LandingEvent::Released { change } => {
                (EventKind::Released, change, json!({ "reason": "landed" }))
            }
        };
        self.0
            .events
            .publish(kind, Some(change.to_string()), detail);
    }
}
