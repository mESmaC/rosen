//! R12 critical contract: reverting one culprit must preserve later lands and
//! the owner's snapshots, and retries must never apply the inverse twice.
use rosen_core::{AgentId, Commit, ObjectId, Signature, Tree};
use rosen_protocol::*;
use rosen_server::{
    landing::{revert, Ledger},
    rpc::dispatch,
    ServerConfig, ServerState,
};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

async fn call(s: &Arc<ServerState>, r: Request) -> Response {
    match dispatch(s, r).await {
        Outcome::Ok(r) => r,
        Outcome::Error(e) => panic!("{e}"),
    }
}

async fn land_file(
    s: &Arc<ServerState>,
    base: ObjectId,
    path: &str,
) -> (rosen_core::Change, ObjectId) {
    let owner = AgentId::new("owner").unwrap();
    let Response::Change(c) = call(s, Request::ChangeNew(NewChange { owner, on: None })).await
    else {
        panic!()
    };
    let id = c.change.id.to_string();
    call(
        s,
        Request::WorkspaceWrite(WorkspaceWrite {
            change: id.clone(),
            path: path.into(),
            data: Some(WorkspaceBytes(b"work\n".to_vec())),
        }),
    )
    .await;
    let Response::Change(c) = call(s, Request::ChangeShow { change: id.clone() }).await else {
        panic!()
    };
    let commit = approve_land(s, id, c.change.head, base).await;
    (c.change, commit)
}

#[tokio::test]
async fn culprit_reopens_once_without_losing_later_work() {
    let temp = tempfile::tempdir().unwrap();
    let (s, store, ledger, root) = setup(temp.path());
    let (original, bad) = land_file(&s, root, "bad.rs").await;
    assert_pending_recovery_blocks_revert(&s, &ledger, bad);
    let (later, tip) = land_file(&s, bad, "later.rs").await;
    let inverse = revert::revert(&s, bad, "tests failed twice").unwrap();
    assert_eq!(revert::revert(&s, bad, "retry").unwrap(), inverse);
    assert_eq!(ledger.base("main").unwrap(), Some(inverse));
    let commit = store.read_commit(&inverse).unwrap();
    assert_eq!(commit.parents, vec![tip]);
    let tree = store.read_tree(&commit.tree).unwrap();
    assert_eq!(
        tree.entries()
            .iter()
            .map(|e| e.name.as_slice())
            .collect::<Vec<_>>(),
        vec![b"later.rs".as_slice()]
    );
    let reopened = s.changes.show(&original.id.to_string()).unwrap().change;
    assert_eq!(reopened.state, rosen_core::ChangeState::Open);
    assert_eq!(reopened.snapshots, original.snapshots);
    assert_eq!(reopened.owner, original.owner);
    assert_eq!(
        revert::failure_output(&s, bad).unwrap().as_deref(),
        Some("tests failed twice")
    );
    assert_eq!(
        s.changes.show(&later.id.to_string()).unwrap().change.state,
        rosen_core::ChangeState::Landed
    );
    assert!(s
        .events
        .page(0, Some(&original.owner), 100)
        .unwrap()
        .events
        .iter()
        .any(|e| e.kind == EventKind::Reverted));
    assert_eq!(ledger.landing_of(&original.id).unwrap(), None);
    let repaired = approve_land(&s, original.id.to_string(), reopened.head, inverse).await;
    assert_eq!(ledger.landing_of(&original.id).unwrap(), Some(repaired));
    assert_eq!(
        revert::revert(&s, bad, "old confirmation").unwrap(),
        inverse
    );
    assert_eq!(ledger.base("main").unwrap(), Some(repaired));
    assert_overlap_batches(&s, bad, repaired);
}

fn assert_pending_recovery_blocks_revert(s: &ServerState, ledger: &Ledger, bad: ObjectId) {
    // R12 risk: an unfinished original worker could re-close the reopened owner.
    let conn = rusqlite::Connection::open(s.config.metadata_path()).unwrap();
    conn.execute(
        "UPDATE base_history SET post_land_done=0 WHERE commit_id=?1",
        [bad.to_hex()],
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TRIGGER hold_post_land BEFORE UPDATE OF post_land_done ON base_history
        WHEN NEW.post_land_done=1 BEGIN SELECT RAISE(ABORT,'injected pending recovery'); END",
    )
    .unwrap();
    assert!(revert::revert(s, bad, "must wait").is_err());
    assert_eq!(ledger.base("main").unwrap(), Some(bad));
    assert_eq!(revert::failure_output(s, bad).unwrap(), None);
    conn.execute_batch("DROP TRIGGER hold_post_land").unwrap();
    s.landing.recover(s).unwrap();
}

fn assert_overlap_batches(s: &ServerState, first: ObjectId, repaired: ObjectId) {
    // R12 risk: the original ID's repaired land must capture fresh R8 targets.
    let conn = rusqlite::Connection::open(s.config.metadata_path()).unwrap();
    let batches: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM landing_overlap_batches WHERE landed IN (?1,?2)",
            rusqlite::params![first.to_hex(), repaired.to_hex()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(batches, 2);
}

async fn approve_land(
    s: &Arc<ServerState>,
    id: String,
    head: ObjectId,
    base: ObjectId,
) -> ObjectId {
    call(
        s,
        Request::ReviewApprove(ApproveRequest {
            change: id.clone(),
            reviewer: AgentId::new("reviewer").unwrap(),
            head,
        }),
    )
    .await;
    call(
        s,
        Request::ReceiptSubmit(SubmitReceipt {
            change: id.clone(),
            head,
            base,
            check_set: "pre-land".into(),
            submitter: AgentId::new("checker").unwrap(),
            signature: None,
        }),
    )
    .await;
    let Response::Land(a) = call(s, Request::Land { change: id }).await else {
        panic!()
    };
    // A land answers before its post-land work runs (issue #190).
    s.landing.settle();
    let LandStatus::Landed { commit, .. } = a.outcome else {
        panic!("{a:?}")
    };
    commit
}

fn setup(path: &std::path::Path) -> (Arc<ServerState>, LooseObjectStore, Ledger, ObjectId) {
    let s = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("revert-contract"),
    });
    let store = LooseObjectStore::open(s.config.objects_dir()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let root = store
        .put_commit(&Commit {
            tree: store.put_tree(&Tree::default()).unwrap(),
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: vec![],
        })
        .unwrap();
    // Exercise adoption of the pre-R12 UNIQUE history schema as well as fresh use.
    let conn = rusqlite::Connection::open(s.config.metadata_path()).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS base_history (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, ref_name TEXT NOT NULL,
        commit_id TEXT NOT NULL, change_id      TEXT UNIQUE, head TEXT,
        tier TEXT NOT NULL, post_land_done INTEGER NOT NULL DEFAULT 0,
        UNIQUE(ref_name,commit_id))",
    )
    .unwrap();
    drop(conn);
    let ledger = Ledger::open(&s.config.metadata_path()).unwrap();
    ledger.init_base("main", &root).unwrap();
    (s, store, ledger, root)
}
