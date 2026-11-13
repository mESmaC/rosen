//! Risk (SPEC R8 / section 3): a failure between SQL writes must not publish
//! a ref without its history and closed change, even on another connection.
use rosen_core::{AgentId, ChangeState, ObjectId, Snapshot, SnapshotKind};
use rosen_server::{changes, landing::*};
use rusqlite::Connection;
fn oid(n: u8) -> ObjectId {
    ObjectId::from_bytes([n; 20])
}

#[test]
fn transaction_fixtures() {
    for row in include_str!("../../../proofs/fixtures/atomic-transactions.tsv")
        .lines()
        .skip(1)
    {
        let flags: Vec<bool> = row.split('\t').map(|v| v == "1").collect();
        let (temp, ledger, mut conn, mut record) = new_attempt();
        let path = temp.path().join("metadata.sqlite");
        if flags[0] {
            ledger.compare_and_swap(&oid(0), &record).unwrap();
        }
        let before = ledger.base("main").unwrap().unwrap();
        record.commit = oid(2);
        if !flags[2] {
            conn.execute_batch(
                "CREATE TRIGGER fail_close BEFORE UPDATE OF state ON changes
                WHEN NEW.state = 'landed' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        }
        let expected = if flags[1] { before } else { oid(9) };
        let result = ledger.compare_and_swap(&expected, &record);
        assert_eq!(
            matches!(result, Ok(CasResult::Swapped { .. })),
            flags[3],
            "{row}"
        );
        drop(ledger);
        let reopened = Ledger::open(&path).unwrap();
        assert_eq!(
            reopened.base("main").unwrap(),
            Some(if flags[3] { oid(2) } else { before })
        );
        let closed = flags[0] || flags[3];
        assert_eq!(
            changes::get(&conn, record.change).unwrap().change.state == ChangeState::Landed,
            closed
        );
        assert_eq!(
            reopened.landing_of(&record.change).unwrap().is_some(),
            closed
        );
        if closed {
            let snapshot = Snapshot {
                commit: oid(8),
                tree: oid(8),
                kind: SnapshotKind::Automatic,
            };
            assert!(changes::record_snapshot(&mut conn, record.change, snapshot).is_err());
        }
    }
}

fn new_attempt() -> (tempfile::TempDir, Ledger, Connection, LandRecord) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("metadata.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    ledger.init_base("main", &oid(0)).unwrap();
    let mut conn = Connection::open(&path).unwrap();
    changes::migrate(&conn).unwrap();
    let change = changes::create(
        &mut conn,
        &AgentId::new("agent").unwrap(),
        changes::NewParent::Base {
            commit: oid(0),
            tree: oid(1),
        },
    )
    .unwrap()
    .change;
    // The swap also needs an approval that still holds (R7).
    let approval = rosen_protocol::ApproveRequest {
        change: change.id.to_string(),
        reviewer: AgentId::new("reviewer").unwrap(),
        head: change.head,
    };
    rosen_server::reviews::approvals::approve(&mut conn, &approval).unwrap();
    let record = LandRecord {
        ref_name: "main".into(),
        change: change.id,
        head: change.head,
        commit: oid(3),
        tier: Tier::ProseOnly,
    };
    (temp, ledger, conn, record)
}
