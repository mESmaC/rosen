//! Risk (SPEC R7): stale review must never authorize new authored work,
//! while a server rebase must preserve review across metadata restarts.
use rosen_core::{AgentId, ChangeParent, ObjectId, Snapshot, SnapshotKind};
use rosen_protocol::ApproveRequest;
use rosen_server::{changes, reviews::approvals};
use rusqlite::Connection;

fn oid(n: u8) -> ObjectId {
    ObjectId::from_slice(&[n; 20]).unwrap()
}

fn open(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    changes::migrate(&conn).unwrap();
    conn
}

#[test]
fn approval_survives_rebase_and_restart_but_not_authored_head_aba() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("metadata.sqlite");
    let mut conn = open(&path);
    let record = changes::create(
        &mut conn,
        &AgentId::new("author").unwrap(),
        changes::NewParent::Base {
            commit: oid(1),
            tree: oid(2),
        },
    )
    .unwrap();
    let id = record.change.id;
    let request = ApproveRequest {
        change: id.to_string(),
        reviewer: AgentId::new("reviewer").unwrap(),
        head: oid(2),
    };
    let approved = approvals::approve(&mut conn, &request).unwrap();
    changes::rebase(&mut conn, id, ChangeParent::Base(oid(3)), oid(4)).unwrap();
    drop(conn);
    let mut conn = open(&path);
    assert_eq!(
        approvals::current(&conn, id, oid(4)).unwrap(),
        Some(approved.clone())
    );
    assert!(approvals::current(&conn, id, oid(2)).unwrap().is_none());
    assert!(approvals::approve(&mut conn, &request).is_err());
    for n in [5, 2] {
        changes::record_snapshot(
            &mut conn,
            id,
            Snapshot {
                commit: oid(n + 10),
                tree: oid(n),
                kind: SnapshotKind::Automatic,
            },
        )
        .unwrap();
    }
    assert!(approvals::current(&conn, id, oid(2)).unwrap().is_none());
    let history = approvals::list(&conn, id).unwrap();
    assert_eq!(history.len(), 1);
    assert!(!history[0].valid);
    let renewed = approvals::approve(&mut conn, &request).unwrap();
    assert!(renewed.valid);
    assert_ne!(renewed.id, approved.id);
    changes::abandon(&mut conn, id).unwrap();
    assert!(approvals::approve(&mut conn, &request).is_err());
}
