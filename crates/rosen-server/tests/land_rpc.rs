//! Risk (SPEC R8 "Accept when"): the base moving without a receipt bound to
//! the change's exact head and to a base the land can still trust, or a land
//! applied twice. Runs the real server path: registry, merge, store, ledger.

use rosen_core::history::LandedTrailers;
use rosen_core::{AgentId, Change, Commit, ObjectId, Signature, Tree};
use rosen_protocol::{
    LandAnswer, LandStatus, NewChange, Outcome, Request, Response, SubmitReceipt, WorkspaceBytes,
    WorkspaceWrite,
};
use rosen_server::landing::{CasResult, LandRecord, Ledger, ReviewGate, Tier};
use rosen_server::{rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

/// Approves every change at its current head, so these tests reach the
/// receipt gate; review's own gate is tested with review.
struct ApproveAll;
impl ReviewGate for ApproveAll {
    fn approval(&self, change: &Change) -> Result<Option<(ObjectId, Vec<AgentId>)>, String> {
        Ok(Some((change.head, vec![AgentId::new("reviewer").unwrap()])))
    }
    fn tier(&self, _: &Change) -> Result<Tier, String> {
        Ok(Tier::Normal)
    }
}

fn setup() -> (tempfile::TempDir, Arc<ServerState>, ObjectId) {
    let temp = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: rosen_protocol::Endpoint::new("land-contract"),
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
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    ledger.init_base("main", &root).unwrap();
    state.landing.set_review_gate(Arc::new(ApproveAll));
    (temp, state, root)
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        Outcome::Error(body) => panic!("{body}"),
    }
}

async fn new_change(state: &Arc<ServerState>) -> String {
    let owner = AgentId::new("agent").unwrap();
    match call(state, Request::ChangeNew(NewChange { owner, on: None })).await {
        Response::Change(record) => record.change.id.to_string(),
        other => panic!("{other:?}"),
    }
}

/// Writes one file and returns the change's new head.
async fn write(state: &Arc<ServerState>, change: &str, path: &str, data: &[u8]) -> ObjectId {
    let request = Request::WorkspaceWrite(WorkspaceWrite {
        change: change.into(),
        path: path.into(),
        data: Some(WorkspaceBytes(data.into())),
    });
    call(state, request).await;
    match call(
        state,
        Request::ChangeShow {
            change: change.into(),
        },
    )
    .await
    {
        Response::Change(record) => record.change.head,
        other => panic!("{other:?}"),
    }
}

async fn receipt(state: &Arc<ServerState>, change: &str, head: ObjectId, base: ObjectId) {
    let request = Request::ReceiptSubmit(SubmitReceipt {
        change: change.into(),
        head,
        base,
        check_set: "pre-land".into(),
        submitter: AgentId::new("checker").unwrap(),
    });
    call(state, request).await;
}

async fn land(state: &Arc<ServerState>, change: &str) -> LandAnswer {
    match call(
        state,
        Request::Land {
            change: change.into(),
        },
    )
    .await
    {
        Response::Land(answer) => answer,
        other => panic!("{other:?}"),
    }
}

fn refused(answer: &LandAnswer) -> Option<&str> {
    match &answer.outcome {
        LandStatus::Refused { reason, .. } => Some(reason),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn base_moves_only_with_a_receipt_for_the_exact_head_and_a_trusted_base() {
    let (_temp, state, root) = setup();
    let (a, b) = (new_change(&state).await, new_change(&state).await);
    let first_head = write(&state, &a, "a.txt", b"one\n").await;
    let head_b = write(&state, &b, "b.txt", b"bee\n").await;
    receipt(&state, &b, head_b, root).await;

    let missing = land(&state, &a).await;
    assert_eq!(
        (refused(&missing), missing.base),
        (Some("no_receipt"), root)
    );

    receipt(&state, &a, first_head, root).await;
    let head_a = write(&state, &a, "a.txt", b"two\n").await;
    let stale = land(&state, &a).await;
    assert_eq!((refused(&stale), stale.base), (Some("receipt_stale"), root));
    // A land that decided on the old head loses its swap once a snapshot
    // moves the head, so the newer edit never closes without reaching the base.
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    let raced = LandRecord {
        ref_name: "main".into(),
        change: a.parse().unwrap(),
        head: first_head,
        commit: root,
        tier: Tier::Normal,
    };
    let swap = ledger.compare_and_swap(&root, &raced).unwrap();
    assert_eq!(
        (swap, ledger.base("main").unwrap()),
        (CasResult::ChangeMoved, Some(root))
    );

    receipt(&state, &a, head_a, root).await;
    let LandStatus::Landed { commit, .. } = land(&state, &a).await.outcome else {
        panic!("a valid receipt on the current base lands");
    };
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let landed = store.read_commit(&commit).unwrap();
    assert_eq!((landed.parents, landed.tree), (vec![root], head_a));
    let trailers = LandedTrailers::parse(&landed.message);
    assert_eq!(trailers.map(|t| t.change.to_string()), Some(a.clone()));
    let again = land(&state, &a).await;
    assert_eq!(again.outcome, LandStatus::AlreadyLanded { commit });

    // B's receipt is bound to the base A's land moved past: it no longer counts.
    let moved = land(&state, &b).await;
    assert_eq!(
        (refused(&moved), moved.base),
        (Some("needs_recheck"), commit)
    );
    receipt(&state, &b, head_b, commit).await;
    let LandStatus::Landed { commit: tip, .. } = land(&state, &b).await.outcome else {
        panic!("a receipt on the moved base lands");
    };
    let merged = store.read_commit(&tip).unwrap();
    assert_eq!(merged.parents, vec![commit]);
    let tree = store.read_tree(&merged.tree).unwrap();
    let names: Vec<_> = tree.entries().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, [b"a.txt".to_vec(), b"b.txt".to_vec()]);
}
