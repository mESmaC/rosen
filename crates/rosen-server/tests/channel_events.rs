//! R9/R13 risk: separate channel sequences or cursors lose messages on
//! reconnect; treating no-change messages as broadcasts leaks targeted lines.
use rosen_core::AgentId;
use rosen_protocol::EventKind;
use rosen_server::events::EventLog;
use serde_json::json;

#[test]
fn channel_and_rosen_events_share_order_audience_and_restart_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("metadata.sqlite");
    let log = EventLog::open(&db);
    let alice = AgentId::new("machine-a/alice").unwrap();
    let bob = AgentId::new("machine-b/bob").unwrap();
    assert_eq!(log.publish_channel(json!({"to":"bob"}), &[bob]), 1);
    assert_eq!(log.publish_channel(json!({"kind":"presence"}), &[]), 2);
    let (_, seq) = log
        .commit_channel(
            &json!({"intent":"request"}),
            std::slice::from_ref(&alice),
            |tx| {
                tx.execute_batch("CREATE TABLE probe (value INTEGER); INSERT INTO probe VALUES (1)")
            },
        )
        .unwrap();
    assert_eq!(seq, 3);
    assert_eq!(log.publish(EventKind::Mirror, None, json!({})), 4);
    let failed = log.commit_channel(&json!({}), &[], |tx| {
        tx.execute("INSERT INTO probe VALUES (2)", [])?;
        Err::<(), _>(rusqlite::Error::InvalidQuery)
    });
    assert!(failed.is_err());
    assert_eq!(log.acknowledge(&alice, 3).unwrap(), 3);
    assert_eq!(log.page(0, None, 100).unwrap().events.len(), 4);
    let visible = log.page(0, Some(&alice), 100).unwrap().events;
    assert_eq!(visible.iter().map(|e| e.seq).collect::<Vec<_>>(), [3, 4]);
    drop(log);
    let reopened = EventLog::open(&db);
    assert_eq!(reopened.cursor(&alice).unwrap(), Some(3));
    assert_eq!(
        reopened.page(3, Some(&alice), 100).unwrap().events[0].seq,
        4
    );
    assert_eq!(reopened.publish_channel(json!({}), &[alice]), 5);
    let conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        conn.query_row("SELECT SUM(value) FROM probe", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
