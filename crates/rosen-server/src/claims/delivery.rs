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

/// Called inside the transaction that removes the records, including on
/// abandon. A rollback therefore keeps both the waits and their holders.
pub(super) fn enqueue(conn: &Connection, outcome: &ReleaseOutcome) -> Result<(), ChangeError> {
    if outcome.released.is_empty() && outcome.woken.is_empty() {
        return Ok(());
    }
    let payload = serde_json::to_string(outcome).map_err(corrupt)?;
    conn.execute(
        "INSERT INTO claim_release_delivery(outcome) VALUES (?1)",
        [payload],
    )?;
    Ok(())
}

/// Replays pending notifications at startup and after each release. Delivery
/// is at least once: a crash after publishing but before deletion can repeat
/// an event, but can never strand a waiter whose record was removed.
pub fn recover(state: &ServerState) -> Result<(), ChangeError> {
    state.changes.with_conn(|conn| {
        let pending = {
            let mut stmt =
                conn.prepare("SELECT id, outcome FROM claim_release_delivery ORDER BY id")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (id, payload) in pending {
            let outcome: ReleaseOutcome = serde_json::from_str(&payload).map_err(corrupt)?;
            if !publish(state, &outcome) {
                return Err(ChangeError::Base(
                    "could not publish claim release; retained for retry".into(),
                ));
            }
            conn.execute(
                "DELETE FROM claim_release_delivery WHERE id = ?1",
                params![id],
            )?;
        }
        Ok(())
    })
}

fn publish(state: &ServerState, outcome: &ReleaseOutcome) -> bool {
    let paths: Vec<String> = outcome.released.iter().map(|r| r.path.clone()).collect();
    let concern = Concern {
        paths: paths.clone(),
        ..Concern::default()
    };
    let detail = json!({ "paths": paths, "woken": outcome.woken });
    if state.events.publish_to(
        EventKind::Released,
        Some(outcome.change.to_string()),
        detail,
        &concern,
    ) == 0
    {
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

fn corrupt(err: serde_json::Error) -> ChangeError {
    ChangeError::Corrupt(format!("claim release delivery: {err}"))
}
