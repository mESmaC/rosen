//! Durable release notifications: deleting a wait must not lose its wake
//! if the process stops before publishing into the event log (SPEC R6).

use crate::{changes::ChangeError, events::Concern, ServerState};
use rosen_protocol::{EventKind, ReleaseOutcome};
use rusqlite::{params, Connection};
use serde_json::json;

pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS claim_release_delivery (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             outcome TEXT NOT NULL
         );",
    )
}

/// Whether the releasing change gets a `released` event of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Releaser {
    Announce,
    /// A land: landing publishes the change's `released` event itself, with
    /// the landed commit, so only the woken waiters hear from here.
    Silent,
}

/// One stored notification: the outcome, and whether to announce the
/// releaser. Rows store the outcome's own JSON, plus `announce_releaser:
/// false` when the releaser stays silent, so rows written before the field
/// existed still read as announcing, as they did then.
struct Pending {
    outcome: ReleaseOutcome,
    announce_releaser: bool,
}

const ANNOUNCE_FIELD: &str = "announce_releaser";

impl Pending {
    fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut value = serde_json::to_value(&self.outcome)?;
        if let (false, Some(fields)) = (self.announce_releaser, value.as_object_mut()) {
            fields.insert(ANNOUNCE_FIELD.into(), false.into());
        }
        serde_json::to_string(&value)
    }

    fn from_json(payload: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(payload)?;
        let announce_releaser = value
            .get(ANNOUNCE_FIELD)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let outcome = serde_json::from_value(value)?;
        Ok(Pending {
            outcome,
            announce_releaser,
        })
    }
}

/// Called inside the transaction that removes the records, including on
/// abandon. A rollback therefore keeps both the waits and their holders.
pub(super) fn enqueue(
    conn: &Connection,
    outcome: &ReleaseOutcome,
    releaser: Releaser,
) -> Result<(), ChangeError> {
    let announce_releaser = releaser == Releaser::Announce && !outcome.released.is_empty();
    if !announce_releaser && outcome.woken.is_empty() {
        return Ok(());
    }
    let pending = Pending {
        outcome: outcome.clone(),
        announce_releaser,
    };
    let payload = pending.to_json().map_err(corrupt)?;
    conn.execute(
        "INSERT INTO claim_release_delivery(outcome) VALUES (?1)",
        [payload],
    )?;
    Ok(())
}

/// Publishes pending notifications after a release has committed. A failure
/// is logged, not returned: the release itself succeeded and its caller
/// cannot undo it. The rows stay stored, and the next release or a restart
/// publishes them.
pub(super) fn deliver(state: &ServerState) {
    if let Err(err) = recover(state) {
        eprintln!("rosen-server: claim release notifications retained for retry: {err}");
    }
}

/// Replays pending notifications at startup and after each release. Delivery
/// is at least once: a crash after publishing but before deletion can repeat
/// an event, but can never strand a waiter whose record was removed.
///
/// Publishes with the change registry unlocked: publishing reads the registry
/// to find the event's audience (R9), so holding it here would deadlock.
pub fn recover(state: &ServerState) -> Result<(), ChangeError> {
    let pending = state.changes.with_conn(|conn| {
        let mut stmt =
            conn.prepare("SELECT id, outcome FROM claim_release_delivery ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    for (id, payload) in pending {
        // A row that cannot be read is left in place for inspection and
        // skipped, so it cannot hold back every later release's waiters.
        let Ok(pending) = Pending::from_json(&payload) else {
            eprintln!("rosen-server: claim release notification {id} is unreadable; skipped");
            continue;
        };
        if !publish(state, &pending) {
            return Err(ChangeError::Base(
                "could not publish claim release; retained for retry".into(),
            ));
        }
        state.changes.with_conn(|conn| {
            conn.execute(
                "DELETE FROM claim_release_delivery WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

fn publish(state: &ServerState, pending: &Pending) -> bool {
    let outcome = &pending.outcome;
    if pending.announce_releaser && !announce_releaser(state, outcome) {
        return false;
    }
    for woken in &outcome.woken {
        // The waiter may itself have closed since the release committed.
        // Retain its original owner rather than depending on live claims.
        let concern = Concern {
            agents: vec![woken.owner.clone()],
            ..Concern::default()
        };
        let detail =
            json!({ "path": woken.path, "released_by": outcome.change.to_string(), "wake": true });
        if state.events.publish_to(
            EventKind::Released,
            Some(woken.change.to_string()),
            detail,
            &concern,
        ) == 0
        {
            return false;
        }
    }
    true
}

/// The releaser's own `released` event, to whoever holds overlapping paths.
fn announce_releaser(state: &ServerState, outcome: &ReleaseOutcome) -> bool {
    let paths: Vec<String> = outcome.released.iter().map(|r| r.path.clone()).collect();
    let concern = Concern {
        paths: paths.clone(),
        ..Concern::default()
    };
    let detail = json!({ "paths": paths, "woken": outcome.woken });
    state.events.publish_to(
        EventKind::Released,
        Some(outcome.change.to_string()),
        detail,
        &concern,
    ) != 0
}

fn corrupt(err: serde_json::Error) -> ChangeError {
    ChangeError::Corrupt(format!("claim release delivery: {err}"))
}
