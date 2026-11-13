//! SPEC R6 risk: a crash between closing a change and notifying its waiters
//! must not strand them, and a failed release must not close the change.
use rosen_core::{AgentId, ChangeId, ObjectId};
use rosen_protocol::{ClaimRequest, Endpoint, Outcome, Request};
use rosen_server::{changes, claims, rpc::dispatch, ServerConfig, ServerState};
use std::sync::Arc;

async fn setup() -> (tempfile::TempDir, Arc<ServerState>, ChangeId, ChangeId) {
    let dir = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: dir.path().into(),
        endpoint: Endpoint::new("release"),
    });
    let mut ids = Vec::new();
    for name in ["holder", "waiter"] {
        let owner = AgentId::new(name).unwrap();
        let parent = changes::NewParent::Base {
            commit: ObjectId::from_bytes([1; 20]),
            tree: ObjectId::from_bytes([2; 20]),
        };
        let record = state
            .changes
            .with_conn(|c| changes::create(c, &owner, parent))
            .unwrap();
        ids.push(record.change.id);
    }
    let req = |id: ChangeId| ClaimRequest {
        change: id.to_string(),
        paths: vec!["src/**".into()],
    };
    assert!(matches!(
        dispatch(&state, Request::ClaimPaths(req(ids[0]))).await,
        Outcome::Ok(_)
    ));
    assert!(matches!(
        dispatch(&state, Request::WaitPaths(req(ids[1]))).await,
        Outcome::Ok(_)
    ));
    (dir, state, ids[0], ids[1])
}

fn count(state: &ServerState, table: &str) -> i64 {
    state
        .changes
        .with_conn(|c| Ok(c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?))
        .unwrap()
}

#[tokio::test]
async fn release_fixtures_cover_rollback_and_restart_delivery() {
    for row in include_str!("../../../proofs/fixtures/claim-release.tsv")
        .lines()
        .skip(1)
    {
        let v: Vec<_> = row.split('\t').map(|c| c.parse::<i64>().unwrap()).collect();
        let (_dir, state, holder, waiter) = setup().await;
        if v[0] == 0 {
            state.changes.with_conn(|c| Ok(c.execute_batch("CREATE TRIGGER fail_release BEFORE DELETE ON path_claims BEGIN SELECT RAISE(ABORT, 'injected'); END;")?)).unwrap();
        }
        assert_eq!(
            state.changes.abandon(&holder.to_string()).is_ok(),
            v[0] == 1
        );
        assert_eq!(count(&state, "path_claims"), if v[2] == 1 { 0 } else { 2 });
        assert_eq!(
            state
                .changes
                .show(&holder.to_string())
                .unwrap()
                .change
                .state
                .is_closed(),
            v[2] == 1
        );
        let config = state.config.clone();
        drop(state);
        let state = ServerState::new(config);
        if v[1] == 1 {
            claims::recover(&state).unwrap();
        }
        assert_eq!(count(&state, "claim_release_delivery"), v[3]);
        let wakes = state
            .events
            .page(0, Some(&AgentId::new("waiter").unwrap()), 100)
            .unwrap();
        assert_eq!(
            wakes
                .iter()
                .filter(|e| e.change.as_deref() == Some(&waiter.to_string())
                    && e.detail["wake"] == true)
                .count(),
            usize::from(v[0] == 1 && v[1] == 1)
        );
    }
}
