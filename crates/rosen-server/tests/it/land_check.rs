//! R8 "Land check" risk: a check that disagrees with the land that follows on
//! the same base sends agents and Velocity after the wrong gate, and a check
//! with side effects marks changes ready or publishes events nobody asked for.
use rosen_core::{AgentId, ChangeState, Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_protocol::*;
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

fn setup(path: &std::path::Path, config: &[u8]) -> (Arc<ServerState>, ObjectId) {
    let state = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("land-check-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let blob = store.put_blob(config).unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "rosen.toml", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let root = store
        .put_commit(&Commit {
            tree,
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"base\n".to_vec(),
        })
        .unwrap();
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    ledger.init_base("main", &root).unwrap();
    (state, root)
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        other => panic!("{other:?}"),
    }
}

async fn change_editing(state: &Arc<ServerState>, path: &str) -> (String, ObjectId) {
    let new = NewChange {
        owner: AgentId::new("owner").unwrap(),
        on: None,
    };
    let Response::Change(record) = call(state, Request::ChangeNew(new)).await else {
        panic!()
    };
    let id = record.change.id.to_string();
    let write = WorkspaceWrite {
        change: id.clone(),
        path: path.into(),
        data: Some(WorkspaceBytes(b"work\n".to_vec())),
    };
    call(state, Request::WorkspaceWrite(write)).await;
    let Response::Change(record) = call(state, Request::ChangeShow { change: id.clone() }).await
    else {
        panic!()
    };
    (id, record.change.head)
}

/// Checks `id`, asserts the check changed nothing, then lands it and asserts
/// the land answers what the check said it would.
async fn check_then_land(state: &Arc<ServerState>, id: &str) -> LandCheck {
    let before = state.changes.show(id).unwrap().change.state;
    let events = state.events.last_seq();
    let Response::LandCheck(check) = call(state, Request::LandCheck { change: id.into() }).await
    else {
        panic!()
    };
    assert_eq!(state.changes.show(id).unwrap().change.state, before);
    assert_eq!(state.events.last_seq(), events);
    assert_eq!(check.landable, check.refusal.is_none());
    let Response::Land(answer) = call(state, Request::Land { change: id.into() }).await else {
        panic!()
    };
    match (&check.refusal, &answer.outcome) {
        (None, LandStatus::Landed { .. }) => {}
        (Some(refusal), LandStatus::Refused { reason, .. }) => assert_eq!(&refusal.reason, reason),
        (expected, outcome) => panic!("check said {expected:?}, land answered {outcome:?}"),
    }
    state.landing.settle();
    *check
}

#[tokio::test(flavor = "multi_thread")]
async fn before_land_check_names_each_gate_the_land_then_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let config = b"[checks.pre_land]\ncommands = [\"build\"]\n";
    let (state, base) = setup(temp.path(), config);
    let (id, head) = change_editing(&state, "src/lib.rs").await;
    assert_eq!(
        state.changes.show(&id).unwrap().change.state,
        ChangeState::Open
    );

    let check = check_then_land(&state, &id).await;
    assert_eq!(check.refusal.unwrap().reason, "unapproved");
    assert_eq!(check.approval, ApprovalGate::Needed);
    assert_eq!(check.receipt, ReceiptGate::Missing);
    assert_eq!(check.merge, MergeGate::Clean);
    assert_eq!((check.base, check.head), (base, head));

    let reviewer = AgentId::new("reviewer").unwrap();
    let approve = ApproveRequest {
        change: id.clone(),
        reviewer,
        head,
    };
    call(&state, Request::ReviewApprove(approve)).await;
    let check = check_then_land(&state, &id).await;
    assert_eq!(check.refusal.unwrap().reason, "no_receipt");
    assert!(matches!(check.approval, ApprovalGate::Held { .. }));

    let receipt = SubmitReceipt {
        change: id.clone(),
        head,
        base,
        check_set: "pre-land".into(),
        submitter: AgentId::new("checker").unwrap(),
        signature: None,
    };
    call(&state, Request::ReceiptSubmit(receipt)).await;
    let check = check_then_land(&state, &id).await;
    assert!(check.landable);
    assert!(matches!(check.receipt, ReceiptGate::Valid { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn after_land_check_needs_no_approval_or_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let (state, _) = setup(temp.path(), b"review = \"after-land\"\n");
    let (id, _) = change_editing(&state, "src/lib.rs").await;
    let check = check_then_land(&state, &id).await;
    assert!(check.landable);
    assert_eq!(check.review_mode, "after-land");
    assert_eq!(check.approval, ApprovalGate::NotRequired);
    assert_eq!(check.receipt, ReceiptGate::NotRequired);
}
