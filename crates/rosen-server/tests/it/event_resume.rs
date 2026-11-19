//! SPEC R9: "Cursors and the event log are stored on the server, so they
//! survive a server restart." Risk: a log or cursor kept only in memory makes
//! every subscriber miss or replay events after a restart, and a sequence that
//! restarts at 1 makes stored cursors skip new events.

use rosen_core::AgentId;
use rosen_protocol::EventKind;
use rosen_server::events::{stage, EventLog, Staged};
use serde_json::json;

#[test]
fn log_and_cursor_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("metadata.sqlite");
    let agent = AgentId::new("alice").unwrap();

    let log = EventLog::open(&db);
    for n in 1..=3 {
        assert_eq!(log.publish(EventKind::Mirror, None, json!(n)), n);
    }
    assert_eq!(log.acknowledge(&agent, 2).unwrap(), 2);
    drop(log);

    let reopened = EventLog::open(&db);
    assert_eq!(reopened.last_seq(), 3);
    assert_eq!(reopened.cursor(&agent).unwrap(), Some(2));
    let unread = reopened.page(2, Some(&agent), 100).unwrap();
    assert_eq!(unread.events.iter().map(|e| e.seq).collect::<Vec<_>>(), [3]);
    // The sequence continues rather than restarting, and a stale ack never
    // moves the cursor back.
    assert_eq!(reopened.publish(EventKind::Mirror, None, json!(4)), 4);
    assert_eq!(reopened.acknowledge(&agent, 1).unwrap(), 2);
}

/// SPEC R9: "Delivery is at least once." Risk: a producer publishes after
/// committing what the event reports, so an event dropped on a failed write
/// is lost for good, and a released claim's waiter never wakes (R6).
#[test]
fn an_event_whose_write_fails_is_stored_later_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("metadata.sqlite");
    let log = EventLog::open(&db);
    assert_eq!(log.publish(EventKind::Mirror, None, json!(1)), 1);

    // Another writer holds the database past the log's busy timeout.
    let blocker = rusqlite::Connection::open(&db).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(log.publish(EventKind::Mirror, None, json!(2)), 0);
    blocker.execute_batch("ROLLBACK").unwrap();

    // The retry stores it without another publish.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while log.last_seq() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(log.publish(EventKind::Mirror, None, json!(3)), 3);
    let details: Vec<_> = log.after(0).into_iter().map(|e| e.detail).collect();
    assert_eq!(details, [json!(1), json!(2), json!(3)]);
}

/// SPEC R9: "Delivery is at least once." Risk: a producer commits its cause
/// on the registry's connection and the event on the log's, so a server that
/// stops between the two loses the event for good (#118). An event staged in
/// the cause's transaction must reach the log after a restart, exactly once
/// however many deliveries run.
#[test]
fn an_event_staged_with_its_cause_survives_a_stop_before_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("metadata.sqlite");
    let producer = rosen_server::metadata::open(&db).unwrap();
    let tx = producer.unchecked_transaction().unwrap();
    let event = Staged {
        kind: EventKind::Mirror,
        change: None,
        detail: json!("staged"),
        concern: Default::default(),
    };
    stage(&tx, &event).unwrap();
    tx.commit().unwrap();
    // The server stops here, before delivering what it staged.
    drop(producer);

    let restarted = EventLog::open(&db);
    restarted.deliver_staged();
    restarted.deliver_staged();
    let details: Vec<_> = restarted.after(0).into_iter().map(|e| e.detail).collect();
    assert_eq!(details, [json!("staged")]);
}
