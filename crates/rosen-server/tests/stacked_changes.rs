//! Risk (R8): updating or landing a parent must preserve every descendant's
//! authored work and commits, while preventing a child from landing first.
use rosen_core::{AgentId, ChangeParent, Commit, Signature, Tree};
use rosen_protocol::{NewChange, Outcome, Request, Response, WorkspaceBytes, WorkspaceWrite};
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

fn setup() -> (tempfile::TempDir, Arc<ServerState>) {
    let temp = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: rosen_protocol::Endpoint::new("stack-contract"),
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let root = store
        .put_commit(&Commit {
            tree: store.put_tree(&Tree::default()).unwrap(),
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"base\n".to_vec(),
        })
        .unwrap();
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .init_base("main", &root)
        .unwrap();
    (temp, state)
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(answer) => answer,
        other => panic!("{other:?}"),
    }
}

async fn create(state: &Arc<ServerState>, on: Option<&str>) -> String {
    let Response::Change(record) = call(
        state,
        Request::ChangeNew(NewChange {
            owner: AgentId::new("owner").unwrap(),
            on: on.map(str::to_owned),
        }),
    )
    .await
    else {
        panic!()
    };
    record.change.id.to_string()
}

async fn write(state: &Arc<ServerState>, id: &str, path: &str, data: &str) {
    call(
        state,
        Request::WorkspaceWrite(WorkspaceWrite {
            change: id.into(),
            path: path.into(),
            data: Some(WorkspaceBytes(data.as_bytes().into())),
        }),
    )
    .await;
}

#[tokio::test]
async fn parent_updates_flow_through_stack_and_parent_lands_first() {
    let (_temp, state) = setup();
    let parent = create(&state, None).await;
    write(&state, &parent, "parent", "one\n").await;
    let child = create(&state, Some(&parent)).await;
    write(&state, &child, "child", "two\n").await;
    let grandchild = create(&state, Some(&child)).await;
    write(&state, &grandchild, "grandchild", "three\n").await;
    let before = state.changes.show(&grandchild).unwrap().change;
    write(&state, &parent, "parent", "updated\n").await;
    let after = state.changes.show(&grandchild).unwrap().change;
    assert_eq!(before.snapshots, after.snapshots);
    assert_ne!(before.head, after.head);
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let tree = store.read_tree(&after.head).unwrap();
    for path in ["parent", "child", "grandchild"] {
        assert!(tree.get(path.as_bytes()).is_some());
    }
    let parent_blob = tree.get(b"parent").unwrap().id;
    assert_eq!(store.read_blob(&parent_blob).unwrap().data, b"updated\n");
    assert_eq!(after.parent, ChangeParent::Change(child.parse().unwrap()));
    let Response::Land(answer) = call(
        &state,
        Request::Land {
            change: child.clone(),
        },
    )
    .await
    else {
        panic!()
    };
    assert!(
        matches!(answer.outcome, rosen_protocol::LandStatus::Refused { ref reason, .. } if reason == "parent_unlanded")
    );
    // Issue #146: a land between stack creation and the parent's land must
    // survive both the child's restack and the child's eventual land.
    let unrelated = create(&state, None).await;
    write(&state, &unrelated, "unrelated", "keep\n").await;
    land_parent(&state, &unrelated).await;
    land_parent(&state, &parent).await;
    assert!(matches!(
        state.changes.show(&child).unwrap().change.parent,
        ChangeParent::Base(_)
    ));
    assert_eq!(
        state.changes.show(&grandchild).unwrap().change.snapshots,
        before.snapshots
    );
    land_parent(&state, &child).await;
    assert_base_keeps_unrelated(&state, &store);
}

fn assert_base_keeps_unrelated(state: &ServerState, store: &dyn ObjectStore) {
    let base = Ledger::open(&state.config.metadata_path())
        .unwrap()
        .base("main")
        .unwrap()
        .unwrap();
    let landed = store.read_commit(&base).unwrap();
    assert!(store
        .read_tree(&landed.tree)
        .unwrap()
        .get(b"unrelated")
        .is_some());
}

#[tokio::test]
async fn pending_parent_update_recovers_and_conflicts_keep_all_sides() {
    let (_temp, state) = setup();
    let parent = create(&state, None).await;
    write(&state, &parent, "shared", "original\n").await;
    let child = create(&state, Some(&parent)).await;
    write(&state, &child, "shared", "child\n").await;
    let before = state.changes.show(&child).unwrap().change;
    // Simulate a crash after a parent snapshot commits and before propagation.
    let other = create(&state, None).await;
    write(&state, &other, "shared", "parent\n").await;
    let incoming = state.changes.show(&other).unwrap().change.head;
    state
        .changes
        .with_conn(|conn| {
            rosen_server::changes::update(conn, parent.parse().unwrap(), |c| {
                c.head = incoming;
                Ok(())
            })
        })
        .unwrap();
    let restarted = ServerState::new(state.config.clone());
    rosen_server::stacks::reconcile(&restarted).unwrap();
    let after = restarted.changes.show(&child).unwrap().change;
    assert_eq!(after.head, before.head);
    assert_eq!(after.snapshots, before.snapshots);
    restarted
        .changes
        .with_conn(|conn| {
            let set = rosen_server::conflicts::load(conn, &child).unwrap();
            let region = set.open().next().unwrap();
            assert_eq!(region.base, b"original\n");
            assert_eq!(region.ours, b"child\n");
            assert_eq!(region.theirs, b"parent\n");
            Ok(())
        })
        .unwrap();
    rosen_server::stacks::reconcile(&restarted).unwrap();
    assert_eq!(restarted.changes.show(&child).unwrap().change, after);
}

#[tokio::test]
async fn stale_child_merge_result_never_reverts_landed_work() {
    // Risk (#146): a checker receipts the merge result. When a child's head
    // still sits on its parent's pre-rebase tree, measuring it from the
    // parent's new base commit turns every intervening land into a deletion.
    let (_temp, state) = setup();
    let parent = create(&state, None).await;
    write(&state, &parent, "shared", "original\n").await;
    let child = create(&state, Some(&parent)).await;
    write(&state, &child, "shared", "child\n").await;
    // The parent update conflicts in the child, so propagation stops there.
    write(&state, &parent, "shared", "parent\n").await;
    let unrelated = create(&state, None).await;
    write(&state, &unrelated, "unrelated", "keep\n").await;
    land_parent(&state, &unrelated).await;
    call(&state, Request::ChangeRebase { change: parent }).await;
    let Response::MergeResult(answer) = call(&state, Request::MergeResult { change: child }).await
    else {
        panic!()
    };
    assert_eq!(answer.conflicts, ["shared"]);
    assert_eq!(answer.tree, None);
}

async fn land_parent(state: &Arc<ServerState>, parent: &str) {
    let head = state.changes.show(parent).unwrap().change.head;
    let base = Ledger::open(&state.config.metadata_path())
        .unwrap()
        .base("main")
        .unwrap()
        .unwrap();
    call(
        state,
        Request::ReviewApprove(rosen_protocol::ApproveRequest {
            change: parent.into(),
            reviewer: AgentId::new("reviewer").unwrap(),
            head,
        }),
    )
    .await;
    call(
        state,
        Request::ReceiptSubmit(rosen_protocol::SubmitReceipt {
            change: parent.into(),
            head,
            base,
            check_set: "pre-land".into(),
            submitter: AgentId::new("checker").unwrap(),
        }),
    )
    .await;
    let Response::Land(answer) = call(
        state,
        Request::Land {
            change: parent.into(),
        },
    )
    .await
    else {
        panic!()
    };
    assert!(matches!(
        answer.outcome,
        rosen_protocol::LandStatus::Landed { .. }
    ));
}
