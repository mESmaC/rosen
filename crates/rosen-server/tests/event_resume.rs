//! SPEC R9: "Cursors and the event log are stored on the server, so they
//! survive a server restart." Risk: a log or cursor kept only in memory makes
//! every subscriber miss or replay events after a restart, and a sequence that
//! restarts at 1 makes stored cursors skip new events.

use rosen_core::AgentId;
use rosen_protocol::EventKind;
use rosen_server::events::EventLog;
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
    assert_eq!(unread.iter().map(|e| e.seq).collect::<Vec<_>>(), [3]);
    // The sequence continues rather than restarting, and a stale ack never
    // moves the cursor back.
    assert_eq!(reopened.publish(EventKind::Mirror, None, json!(4)), 4);
    assert_eq!(reopened.acknowledge(&agent, 1).unwrap(), 2);
}
