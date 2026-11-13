//! Risk (R12): a stale checker must not green future lands or regress the
//! durable green prefix; a batch must notify every covered change.
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
    };
    call(state, Request::ReceiptSubmit(receipt)).await;
    let approve = ApproveRequest {
        change: change.into(),
        head,
        reviewer: AgentId::new("reviewer").unwrap(),
    };
    call(state, Request::ReviewApprove(approve)).await;
}

#[tokio::test]
async fn batch_green_survives_restart_and_excludes_future_lands() {
    let (_temp, state) = setup();
    assert!(next(&state).await.job.is_none());
    let a = land(&state, 1).await;
    let old = next(&state).await.job.unwrap();
    assert_eq!(old.commit, a);
    let b = land(&state, 2).await;
    let batch = next(&state).await.job.unwrap();
    let c = land(&state, 3).await;
    let mut wrong = batch.clone();
    wrong.check_set.commands.push("skip tests".into());
    assert!(matches!(
        dispatch(&state, Request::PostLandPassed(wrong)).await,
        Outcome::Error(_)
    ));
    assert_eq!(next(&state).await.last_green, None);
    call(&state, Request::PostLandPassed(batch.clone())).await;
    call(&state, Request::PostLandPassed(old)).await;
    call(&state, Request::PostLandPassed(batch)).await;
    let config = state.config.clone();
    drop(state);
    let restarted = ServerState::new(config);
    let work = next(&restarted).await;
    assert_eq!(work.last_green, Some(b));
    assert_eq!(work.job.unwrap().commit, c);
    let events = restarted
        .events
        .page(0, Some(&AgentId::new("owner").unwrap()), 100)
        .unwrap();
    let greens: Vec<_> = events
        .events
        .iter()
        .filter(|e| e.kind == EventKind::Green)
        .collect();
    assert_eq!(greens.len(), 2);
    assert_eq!(greens[0].detail["commit"], a.to_string());
    assert_eq!(greens[1].detail["commit"], b.to_string());
}

// Risk (R12): the executable SQL prefix update must agree with the proven
// model on first, repeated and out-of-order passes.
#[test]
fn sql_prefix_matches_lean_fixtures() {
    use rosen_server::post_land::store;
    let fixture = include_str!("../../../proofs/fixtures/batch-green.tsv");
    for line in fixture.lines().skip(1) {
        let cells: Vec<u32> = line.split('\t').map(|n| n.parse().unwrap()).collect();
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        rosen_server::metadata::ensure_schema(&conn).unwrap();
        store::ensure_schema(&conn).unwrap();
        for n in 1..=3 {
            conn.execute(
                "INSERT INTO base_history(seq,ref_name,commit_id,change_id,tier)
                 VALUES (?1,'main',?2,?3,'normal')",
                rusqlite::params![n, format!("{n:040x}"), format!("change{n}")],
            )
            .unwrap();
        }
        let commit = |n: u32| format!("{n:040x}").parse::<ObjectId>().unwrap();
        if cells[0] > 0 {
            assert!(store::passed(&mut conn, "main", &commit(cells[0])).unwrap());
        }
        assert!(store::passed(&mut conn, "main", &commit(cells[1])).unwrap());
        assert_eq!(
            store::last_green(&conn, "main").unwrap(),
            Some(commit(cells[2]))
        );
        let pending = store::pending(&conn).unwrap();
        assert_eq!(pending.len(), cells[2] as usize);
    }
}
