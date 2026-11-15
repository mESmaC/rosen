//! Risk (R8): atomic stacks must keep per-change history and count only
//! checks of the final tree, including after a separate parent land.
use rosen_core::{AgentId, Commit, Signature, Tree};
use rosen_protocol::{NewChange, Outcome, Request, Response, WorkspaceBytes, WorkspaceWrite};
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

fn setup() -> (tempfile::TempDir, Arc<ServerState>) {
    let temp = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: rosen_protocol::Endpoint::new("stack-contract"),
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

async fn approve(state: &Arc<ServerState>, id: &str) {
    let head = state.changes.show(id).unwrap().change.head;
    call(
        state,
        Request::ReviewApprove(rosen_protocol::ApproveRequest {
            change: id.into(),
            head,
            reviewer: AgentId::new("reviewer").unwrap(),
        }),
    )
    .await;
}

async fn receipt(state: &Arc<ServerState>, id: &str) -> rosen_core::ObjectId {
    let head = state.changes.show(id).unwrap().change.head;
    let base = Ledger::open(&state.config.metadata_path())
        .unwrap()
        .base("main")
        .unwrap()
        .unwrap();
    call(
        state,
        Request::ReceiptSubmit(rosen_protocol::SubmitReceipt {
            change: id.into(),
            head,
            base,
            check_set: "pre-land".into(),
            submitter: AgentId::new("checker").unwrap(),
            signature: None,
        }),
    )
    .await;
    head
}

async fn land(state: &Arc<ServerState>, id: &str) -> rosen_core::ObjectId {
    let started = std::time::Instant::now();
    let Response::Land(answer) = call(state, Request::Land { change: id.into() }).await else {
        panic!("land response");
    };
    let elapsed = started.elapsed();
    let rosen_protocol::LandStatus::Landed { commit, .. } = answer.outcome else {
        panic!("{:?}", answer.outcome);
    };
    eprintln!("stack land took {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "{elapsed:?}"
    );
    commit
}

#[tokio::test]
async fn atomic_stack_and_receipt_reuse_match_proved_fixtures() {
    // R8: a receipt on the top covers one atomic request, never a lost
    // prefix or a squashed history that prevents single-culprit bisection.
    let fixtures = include_str!("../../../proofs/fixtures/stacked-changes.tsv");
    let mut checked = 0;
    for row in fixtures
        .lines()
        .filter(|row| row.starts_with("stack_land\t"))
    {
        let cells: Vec<_> = row.split('\t').collect();
        let depth: usize = cells[1].parse().unwrap();
        assert_eq!(cells[2].parse::<usize>().unwrap(), depth);
        assert_eq!(cells[3].parse::<usize>().unwrap(), depth);
        assert_eq!(cells[4], "1");
        checked += 1;
        let (_temp, state) = setup();
        let mut ids = Vec::<String>::new();
        for index in 0..depth {
            let id = create(&state, ids.last().map(String::as_str)).await;
            write(&state, &id, &format!("layer{index}.rs"), "checked\n").await;
            approve(&state, &id).await;
            ids.push(id);
        }
        let snapshots: Vec<_> = ids
            .iter()
            .map(|id| state.changes.show(id).unwrap().change.snapshots)
            .collect();
        let top = ids.last().unwrap();
        receipt(&state, top).await;
        if depth == 2 {
            // R8 also retains independent-land reuse for the whole stack.
            land_independent(&state).await;
        }
        let tip = land(&state, top).await;
        assert_chain(&state, &ids, &snapshots, tip);
        state.landing.settle();
    }
    assert_eq!(checked, 2);
    receipt_survives_parent_land(false).await;
    receipt_survives_parent_land(true).await;
}

fn assert_chain(
    state: &ServerState,
    ids: &[String],
    snapshots: &[Vec<rosen_core::Snapshot>],
    mut commit: rosen_core::ObjectId,
) {
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    assert_eq!(ledger.base("main").unwrap(), Some(commit));
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    for (id, snapshots) in ids.iter().zip(snapshots).rev() {
        let change = state.changes.show(id).unwrap().change;
        assert_eq!(change.state, rosen_core::ChangeState::Landed);
        assert_eq!(ledger.landing_of(&change.id).unwrap(), Some(commit));
        assert_eq!(change.snapshots, *snapshots);
        let landed = store.read_commit(&commit).unwrap();
        let trailers = rosen_core::history::LandedTrailers::parse(&landed.message).unwrap();
        assert_eq!(trailers.change.to_string(), *id);
        assert_eq!(landed.parents.len(), 1);
        commit = landed.parents[0];
    }
}

async fn land_independent(state: &Arc<ServerState>) {
    let other = create(state, None).await;
    write(state, &other, "other.md", "independent\n").await;
    approve(state, &other).await;
    land(state, &other).await;
}

fn assert_preview_binding(state: &ServerState, preview: &rosen_protocol::MergeResultAnswer) {
    // R8: a second preview must never retarget an in-flight check's binding.
    assert_ne!(preview.tree, Some(preview.head));
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    assert!(ledger
        .remember_merge_result(&preview.change, &preview.head, &preview.base, &preview.head,)
        .is_err());
}

async fn receipt_survives_parent_land(late: bool) {
    let (_temp, state) = setup();
    let parent = create(&state, None).await;
    write(&state, &parent, "parent.rs", "parent\n").await;
    let child = create(&state, Some(&parent)).await;
    write(&state, &child, "child.rs", "child\n").await;
    land_independent(&state).await;
    approve(&state, &parent).await;
    approve(&state, &child).await;
    let Response::MergeResult(preview) = call(
        &state,
        Request::MergeResult {
            change: child.clone(),
        },
    )
    .await
    else {
        panic!("merge preview");
    };
    assert_preview_binding(&state, &preview);
    let checked = rosen_protocol::SubmitReceipt {
        change: child.clone(),
        head: preview.head,
        base: preview.base,
        check_set: "pre-land".into(),
        submitter: AgentId::new("checker").unwrap(),
        signature: None,
    };
    if !late {
        call(&state, Request::ReceiptSubmit(checked.clone())).await;
    }
    receipt(&state, &parent).await;
    land(&state, &parent).await;
    assert_ne!(
        state.changes.show(&child).unwrap().change.head,
        preview.head
    );
    if late {
        call(&state, Request::ReceiptSubmit(checked)).await;
    }
    land(&state, &child).await;
    state.landing.settle();
}
