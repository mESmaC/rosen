//! Risk (SPEC R2 accept): "a change's commits survive any number of rebases
//! unchanged" and "the change ID stays the same for its whole life". If a
//! rebase or snapshot re-keyed a change or rewrote its commit list, every
//! claim, review and receipt bound to it would silently detach.

use rosen_core::change::{AgentId, ChangeParent, ChangeState};
use rosen_core::object::ObjectId;
use rosen_server::changes::{
    self, BaseSource, BaseTip, ChangeError, ChangeRequest, ChangeResponse,
};
use rusqlite::Connection;

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes([byte; 20])
}

struct FixedBase;
impl BaseSource for FixedBase {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(BaseTip {
            commit: oid(1),
            tree: oid(2),
        })
    }
}

fn new_change(conn: &mut Connection, on: Option<String>) -> changes::ChangeRecord {
    let request = ChangeRequest::New {
        owner: AgentId::new("agent-7"),
        on,
    };
    match changes::handle(conn, &FixedBase, request).unwrap() {
        ChangeResponse::Change(record) => record,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn id_and_commits_survive_snapshots_and_rebases() {
    let mut conn = Connection::open_in_memory().unwrap();
    changes::migrate(&conn).unwrap();
    let lower = new_change(&mut conn, None);
    let upper = new_change(&mut conn, Some(lower.id.to_string()[..6].to_string()));
    assert_eq!(upper.parent, ChangeParent::Change(lower.id));
    assert_eq!(upper.head_tree, oid(2));
    let cycle = changes::rebase(&mut conn, lower.id, ChangeParent::Change(upper.id), oid(3));
    assert!(matches!(cycle, Err(ChangeError::Cycle { .. })));

    for n in 0..5 {
        changes::append_commit(&mut conn, upper.id, oid(10 + n), oid(20 + n)).unwrap();
    }
    let before = changes::get(&conn, upper.id).unwrap().commits;
    for n in 0..20 {
        let rebased = changes::rebase(
            &mut conn,
            upper.id,
            ChangeParent::Base(oid(100 + n)),
            oid(200),
        )
        .unwrap();
        assert_eq!((rebased.id, &rebased.commits), (upper.id, &before));
    }

    let abandoned = changes::abandon(&mut conn, lower.id).unwrap();
    assert_eq!(abandoned.state, ChangeState::Abandoned);
    let on_closed = changes::create(
        &mut conn,
        &AgentId::new("a"),
        changes::NewParent::Change(lower.id),
    );
    assert!(matches!(on_closed, Err(ChangeError::Closed { .. })));
    assert!(matches!(
        changes::resolve(&conn, "abc"),
        Err(ChangeError::InvalidId(_))
    ));
}
