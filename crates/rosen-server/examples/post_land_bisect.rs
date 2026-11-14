//! Run the complete failing-batch, bisection, crash recovery and inverse land path.
use rosen_core::{AgentId, Commit, ObjectId, Signature, Tree};
use rosen_protocol::*;
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

fn setup() -> (tempfile::TempDir, Arc<ServerState>) {
    let temp = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: Endpoint::new("green-contract"),
        warm_pool_dir: None,
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
        Outcome::Ok(response) => response,
        other => panic!("{other:?}"),
    }
}

async fn next(state: &Arc<ServerState>) -> PostLandWork {
    match call(state, Request::PostLandNext).await {
        Response::PostLand(work) => work,
        other => panic!("{other:?}"),
    }
}

async fn land(state: &Arc<ServerState>, n: u8) -> ObjectId {
    let create = NewChange {
        owner: AgentId::new("owner").unwrap(),
        on: None,
    };
    let Response::Change(record) = call(state, Request::ChangeNew(create)).await else {
        panic!()
    };
    let change = record.change.id.to_string();
    let write = WorkspaceWrite {
        change: change.clone(),
        path: format!("file{n}.rs"),
        data: Some(WorkspaceBytes(vec![n])),
    };
    call(state, Request::WorkspaceWrite(write)).await;
    let show = Request::ChangeShow {
        change: change.clone(),
    };
    let Response::Change(record) = call(state, show).await else {
        panic!()
    };
    let base = current_base(state);
    approve_and_receipt(state, &change, record.change.head, base).await;
    let Response::Land(answer) = call(state, Request::Land { change }).await else {
        panic!()
    };
    let LandStatus::Landed { commit, .. } = answer.outcome else {
        panic!("{answer:?}")
    };
    commit
}

fn current_base(state: &ServerState) -> ObjectId {
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .project_base()
        .unwrap()
        .unwrap()
        .1
}

async fn approve_and_receipt(
    state: &Arc<ServerState>,
    change: &str,
    head: ObjectId,
    base: ObjectId,
) {
    let receipt = SubmitReceipt {
        change: change.into(),
        head,
        base,
        check_set: "pre-land".into(),
        submitter: AgentId::new("checker").unwrap(),
        signature: None,
    };
    call(state, Request::ReceiptSubmit(receipt)).await;
    let approve = ApproveRequest {
        change: change.into(),
        head,
        reviewer: AgentId::new("reviewer").unwrap(),
    };
    call(state, Request::ReviewApprove(approve)).await;
}

#[tokio::main]
async fn main() {
    let (_temp, mut state) = setup();
    let good = land(&state, 1).await;
    let passed = next(&state).await.job.unwrap();
    call(&state, Request::PostLandPassed(passed)).await;
    let bad = land(&state, 2).await;
    let later = land(&state, 3).await;
    let job = next(&state).await.job.unwrap();
    let Response::Bisection(mut progress) = call(
        &state,
        Request::PostLandFailed {
            job,
            output: "injected batch failure".into(),
        },
    )
    .await
    else {
        panic!()
    };
    while let Some(job) = progress.job {
        let result = BisectResult {
            passed: job.commit == good,
            job,
            output: "injected culprit failure".into(),
        };
        // Commit the confirmation directly, simulating a crash before inverse land.
        let mut conn = rosen_server::metadata::open(&state.config.metadata_path()).unwrap();
        progress = rosen_server::bisect::submit(&mut conn, &result).unwrap();
        let config = state.config.clone();
        drop(state);
        state = ServerState::new(config);
        rosen_server::bisect::recover(&state).unwrap();
        println!("resumed bisection {}", progress.id);
    }
    let culprit = progress.culprit.unwrap();
    assert_eq!(culprit.commit, bad);
    let Response::Change(record) = call(
        &state,
        Request::ChangeShow {
            change: culprit.change,
        },
    )
    .await
    else {
        panic!()
    };
    assert_eq!(record.change.state, rosen_core::ChangeState::Open);
    let inverse = current_base(&state);
    assert_ne!(inverse, later);
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let commit = store.read_commit(&inverse).unwrap();
    assert_eq!(commit.parents, vec![later]);
    let tree = store.read_tree(&commit.tree).unwrap();
    assert!(tree.entries().iter().any(|e| e.name == b"file3.rs"));
    assert!(!tree.entries().iter().any(|e| e.name == b"file2.rs"));
    rosen_server::bisect::recover(&state).unwrap();
    assert_eq!(current_base(&state), inverse);
    println!("culprit reopened, later land preserved, recovery inverse applied exactly once");
}
