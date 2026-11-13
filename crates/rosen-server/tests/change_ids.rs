//! Risk (SPEC R2 accept): "a change's commits survive any number of rebases
//! unchanged" and "the change ID stays the same for its whole life". If a
//! rebase or snapshot re-keyed a change or rewrote its commit list, every
//! claim, review and receipt bound to it would silently detach.

use rosen_core::{AgentId, ChangeParent, ChangeState, ObjectId, Snapshot, SnapshotKind};
use rosen_protocol::NewChange;
use rosen_server::changes::{self, BaseSource, BaseTip, ChangeError, ChangeService};
use std::sync::Arc;

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

fn new_change(service: &ChangeService, on: Option<String>) -> rosen_core::Change {
    let owner = AgentId::new("agent-7").unwrap();
    service.create(&NewChange { owner, on }).unwrap().change
}

#[test]
fn id_and_commits_survive_snapshots_and_rebases() {
    let service = ChangeService::in_memory();
    service.set_base_source(Arc::new(FixedBase));
    let lower = new_change(&service, None);
    let upper = new_change(&service, Some(lower.id.to_string()[..6].to_string()));
    assert_eq!(
        (upper.parent, upper.head),
        (ChangeParent::Change(lower.id), oid(2))
    );

    service
        .with_conn(|conn| {
            let cycle = changes::rebase(conn, lower.id, ChangeParent::Change(upper.id), oid(3));
            assert!(matches!(cycle, Err(ChangeError::Refused(_))));
            for n in 0..5 {
                let kind = if n % 2 == 0 {
                    SnapshotKind::Automatic
                } else {
                    SnapshotKind::Named(format!("step {n}"))
                };
                let snapshot = Snapshot {
                    commit: oid(10 + n),
                    tree: oid(20 + n),
                    kind,
                };
                changes::record_snapshot(conn, upper.id, snapshot)?;
            }
            let before = changes::get(conn, upper.id)?.change.snapshots;
            for n in 0..20 {
                let rebased =
                    changes::rebase(conn, upper.id, ChangeParent::Base(oid(100 + n)), oid(200))?;
                assert_eq!(
                    (rebased.change.id, &rebased.change.snapshots),
                    (upper.id, &before)
                );
            }
            Ok(())
        })
        .unwrap();

    assert_eq!(
        service.abandon(&lower.id.to_string()).unwrap().change.state,
        ChangeState::Abandoned
    );
    let on_closed = service.create(&NewChange {
        owner: AgentId::new("a").unwrap(),
        on: Some(lower.id.to_string()),
    });
    assert!(matches!(on_closed, Err(ChangeError::ParentClosed { .. })));
    assert!(matches!(
        service.show("abc"),
        Err(ChangeError::InvalidId(_))
    ));
}
