//! Risk (SPEC R8 "Accept when"): the base moving without a receipt bound to
//! the change's exact head and to a base the land can still trust, or a land
//! applied twice. Runs the real server path: registry, merge, store, ledger.

use rosen_core::history::LandedTrailers;
use rosen_core::{AgentId, Commit, ObjectId, ReceiptTrailer, Signature, Tree};
use rosen_protocol::{
    ApproveRequest, ErrorCode, LandAnswer, LandStatus, LogRequest, NewChange, Outcome, Request,
    ResolveConflicts, Response, SubmitReceipt, WorkspaceBytes, WorkspaceWrite,
};
use rosen_server::landing::{CasResult, LandRecord, Ledger, Tier};
use rosen_server::{rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

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

async fn show(state: &Arc<ServerState>, change: &str) -> rosen_core::Change {
    let request = Request::ChangeShow {
        change: change.into(),
    };
    match call(state, request).await {
        Response::Change(record) => record.change,
        other => panic!("{other:?}"),
    }
}

async fn approve(state: &Arc<ServerState>, change: &str, head: ObjectId) {
    let request = Request::ReviewApprove(ApproveRequest {
        change: change.into(),
        reviewer: AgentId::new("reviewer").unwrap(),
        head,
    });
    call(state, request).await;
}

/// Approves the change at its current head, while it can still take one,
/// then lands it: these tests reach the gates after review.
async fn land(state: &Arc<ServerState>, change: &str) -> LandAnswer {
    let current = show(state, change).await;
    if !current.state.is_closed() {
        approve(state, change, current.head).await;
    }
    land_as_is(state, change).await
}

async fn land_as_is(state: &Arc<ServerState>, change: &str) -> LandAnswer {
    let request = Request::Land {
        change: change.into(),
    };
    match call(state, request).await {
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

// Risk (R8 step 1): prose may skip the receipt only; it must retain approval,
// and editing the check-set config must not exempt code in the same change.
#[tokio::test(flavor = "multi_thread")]
async fn prose_skips_only_receipts_and_base_selects_the_check_set() {
    let (_temp, state, _) = setup();
    let prose = new_change(&state).await;
    write(&state, &prose, "README.md", b"Guide\n").await;
    assert_eq!(refused(&land_as_is(&state, &prose).await), Some("unapproved"));
    assert!(matches!(land(&state, &prose).await.outcome, LandStatus::Landed { .. }));
    let change = new_change(&state).await;
    let config = b"[checks.pre_land]\nname='build-lint'\ncommands=['build','lint']\n";
    let head_config = write(&state, &change, "rosen.toml", config).await;
    assert_eq!(refused(&land(&state, &change).await), Some("no_receipt"));
    let base = Ledger::open(&state.config.metadata_path()).unwrap().base("main").unwrap().unwrap();
    receipt(&state, &change, head_config, base).await;
    assert!(matches!(land(&state, &change).await.outcome, LandStatus::Landed { .. }));
    let code = new_change(&state).await;
    write(&state, &code, "code.rs", b"fn main() {}\n").await;
    let answer = land(&state, &code).await;
    assert_eq!(refused(&answer), Some("no_receipt"));
    let LandStatus::Refused { message, .. } = answer.outcome else { unreachable!() };
    assert!(message.contains("`build-lint`"));
}

// Risk: the production adapter must reuse independent receipts while still
// refusing clean textual merges whose changes share a build unit (SPEC R8).
#[tokio::test(flavor = "multi_thread")]
async fn moved_base_reuses_only_independent_receipts_through_server() {
    let (_temp, state, root) = setup();
    let configure = new_change(&state).await;
    let config = b"[build_units.a]\npaths=['a*']\n[build_units.b]\npaths=['b*']\n";
    let head = write(&state, &configure, "rosen.toml", config).await;
    receipt(&state, &configure, head, root).await;
    let LandStatus::Landed { commit: base, .. } = land(&state, &configure).await.outcome else {
        panic!("configuration must land");
    };
    let a = new_change(&state).await;
    let b = new_change(&state).await;
    let c = new_change(&state).await;
    let ah = write(&state, &a, "a.rs", b"a\n").await;
    let bh = write(&state, &b, "b.rs", b"b\n").await;
    let ch = write(&state, &c, "another.rs", b"c\n").await;
    for (change, head) in [(&a, ah), (&b, bh), (&c, ch)] {
        receipt(&state, change, head, base).await;
    }
    assert!(matches!(
        land(&state, &a).await.outcome,
        LandStatus::Landed { .. }
    ));
    let LandStatus::Landed { commit: tip, .. } = land(&state, &b).await.outcome else {
        panic!("independent receipt must survive the moved base");
    };
    let refused_c = land(&state, &c).await;
    assert_eq!(
        (refused(&refused_c), refused_c.base),
        (Some("needs_recheck"), tip)
    );
    receipt(&state, &c, ch, tip).await;
    assert!(matches!(
        land(&state, &c).await.outcome,
        LandStatus::Landed { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn base_moves_only_with_a_receipt_for_the_exact_head_and_a_trusted_base() {
    let (_temp, state, root) = setup();
    let (a, b) = (new_change(&state).await, new_change(&state).await);
    let first_head = write(&state, &a, "a.rs", b"one\n").await;
    let head_b = write(&state, &b, "b.rs", b"bee\n").await;
    receipt(&state, &b, head_b, root).await;

    let missing = land(&state, &a).await;
    assert_eq!(
        (refused(&missing), missing.base),
        (Some("no_receipt"), root)
    );

    receipt(&state, &a, first_head, root).await;
    let head_a = write(&state, &a, "a.rs", b"two\n").await;
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
    // D15: the landed commit names its change, the receipt that counted and
    // its approver, and the log reads them back from the commit.
    let trailers = LandedTrailers::parse(&landed.message).unwrap();
    let checker = AgentId::new("checker").unwrap();
    let expected = ReceiptTrailer::new("pre-land", head_a, root, checker).unwrap();
    assert_eq!(trailers.change.to_string(), a);
    assert_eq!(trailers.receipt.as_ref(), Some(&expected));
    let Response::Log(log) = call(&state, Request::Log(LogRequest::default())).await else {
        panic!("log answers with a log");
    };
    let entry = &log.entries[0];
    assert_eq!(
        (entry.commit, entry.change),
        (commit, Some(trailers.change))
    );
    assert_eq!(entry.receipt.as_ref(), Some(&expected));
    assert_eq!(entry.approvers, [AgentId::new("reviewer").unwrap()]);
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
    assert_eq!(names, [b"a.rs".to_vec(), b"b.rs".to_vec()]);
}

// Risk (SPEC R8 step 2): a refused land must durably retain the owner's
// conflicting edits and name their files without moving the base.
#[tokio::test(flavor = "multi_thread")]
async fn conflicted_land_routes_regions_and_files_to_its_owner() {
    let (temp, state, root) = setup();
    let (a, b) = (new_change(&state).await, new_change(&state).await);
    let ha = write(&state, &a, "shared.rs", b"first\n").await;
    receipt(&state, &a, ha, root).await;
    let tip = land(&state, &a).await.base;
    // Edit after A lands: B had no overlap to rebase then, so its own land
    // still exercises R8 step 2 conflict routing against a moved base.
    let hb = write(&state, &b, "shared.rs", b"second\n").await;
    receipt(&state, &b, hb, root).await;
    let answer = land(&state, &b).await;
    assert_eq!(answer.base, tip);
    assert_eq!(refused(&answer), Some("merge_conflict"));
    let LandStatus::Refused { paths, .. } = answer.outcome else {
        unreachable!()
    };
    assert_eq!(paths, ["shared.rs"]);
    let events = state.events.after(0);
    let event = events
        .iter()
        .find(|e| e.kind == rosen_protocol::EventKind::Conflict)
        .unwrap();
    assert_eq!(event.change.as_deref(), Some(b.as_str()));
    assert_eq!(event.detail["paths"], serde_json::json!(["shared.rs"]));
    let conn = rusqlite::Connection::open(state.config.metadata_path()).unwrap();
    let set = rosen_server::conflicts::load(&conn, &b).unwrap();
    let region = set.open().next().unwrap();
    assert_eq!(
        (&region.base, &region.ours, &region.theirs),
        (&Vec::new(), &b"second\n".to_vec(), &b"first\n".to_vec())
    );
    assert!(rosen_server::conflicts::check_landable(&conn, &b).is_err());
    assert!(rosen_server::conflicts::load(&conn, &a)
        .unwrap()
        .open()
        .next()
        .is_none());
    drop(state);
    let restarted = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: rosen_protocol::Endpoint::new("restart"),
    });
    let record = restarted.changes.show(&b).unwrap().change;
    assert_eq!(record.head, hb);
    assert_eq!(record.conflicts, vec![region.id]);
}

// Risk (SPEC R5, R8): `rosen resolve` is the only way a conflicted change
// lands again. Resolving must clear the gate, yet resolving a region that
// still fails to merge must reopen it rather than land a silent pick.
#[tokio::test(flavor = "multi_thread")]
async fn resolved_conflicts_land_only_once_the_file_merges() {
    let (_temp, state, root) = setup();
    let (a, b) = (new_change(&state).await, new_change(&state).await);
    let ha = write(&state, &a, "shared.rs", b"first\n").await;
    receipt(&state, &a, ha, root).await;
    let tip = land(&state, &a).await.base;
    let hb = write(&state, &b, "shared.rs", b"second\n").await;
    receipt(&state, &b, hb, tip).await;
    assert_eq!(refused(&land(&state, &b).await), Some("merge_conflict"));
    assert_eq!(refused(&land(&state, &b).await), Some("conflicts"));

    let resolve = |paths: &[&str]| {
        let request = Request::ConflictResolve(ResolveConflicts {
            change: b.clone(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
        });
        dispatch(&state, request)
    };
    let Outcome::Error(unknown) = resolve(&["other.rs"]).await else {
        panic!("a path with no stored conflict must fail");
    };
    assert_eq!(unknown.code, ErrorCode::NotFound);
    let Outcome::Ok(Response::ConflictsResolved(answer)) = resolve(&[r".\shared.rs"]).await else {
        panic!("resolving the conflicted file must succeed");
    };
    assert_eq!(
        (answer.resolved[0].conflicts, answer.open_paths.len()),
        (1, 0)
    );

    // Resolved without fixing the file: the land merges again and reopens it.
    assert_eq!(refused(&land(&state, &b).await), Some("merge_conflict"));
    assert_eq!(refused(&land(&state, &b).await), Some("conflicts"));

    // The owner's fix: take the base's line, and keep B's own work elsewhere.
    write(&state, &b, "shared.rs", b"first\n").await;
    let head = write(&state, &b, "b.rs", b"b\n").await;
    receipt(&state, &b, head, tip).await;
    let Outcome::Ok(Response::ConflictsResolved(answer)) = resolve(&["shared.rs"]).await else {
        panic!("resolving the reopened conflict must succeed");
    };
    assert!(answer.open_paths.is_empty());
    assert!(matches!(
        land(&state, &b).await.outcome,
        LandStatus::Landed { .. }
    ));
}

// Risk (SPEC R7 "Accept when"): a change lands only with an approval for its
// current head. An author snapshot voids it for good, even one that puts the
// head back where the approval and a racing land read it.
#[tokio::test(flavor = "multi_thread")]
async fn a_land_needs_an_approval_that_still_holds_for_the_current_head() {
    let (_temp, state, root) = setup();
    let a = new_change(&state).await;
    let first = write(
        &state, &a, "a.rs", b"one
",
    )
    .await;
    receipt(&state, &a, first, root).await;
    assert_eq!(refused(&land_as_is(&state, &a).await), Some("unapproved"));

    approve(&state, &a, first).await;
    write(
        &state, &a, "a.rs", b"two
",
    )
    .await;
    assert_eq!(
        write(
            &state, &a, "a.rs", b"one
"
        )
        .await,
        first
    );
    assert_eq!(refused(&land_as_is(&state, &a).await), Some("unapproved"));
    // A land that read the approval before the author's snapshots loses its
    // swap, though the head it decided on is the head again.
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    let raced = LandRecord {
        ref_name: "main".into(),
        change: a.parse().unwrap(),
        head: first,
        commit: root,
        tier: Tier::Normal,
    };
    let swap = ledger.compare_and_swap(&root, &raced).unwrap();
    assert_eq!(
        (swap, ledger.base("main").unwrap()),
        (CasResult::ChangeMoved, Some(root))
    );

    approve(&state, &a, first).await;
    let LandStatus::Landed { commit, .. } = land_as_is(&state, &a).await.outcome else {
        panic!("an approval and a receipt for the current head land");
    };
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let trailers = LandedTrailers::parse(&store.read_commit(&commit).unwrap().message).unwrap();
    assert_eq!(trailers.approvers, [AgentId::new("reviewer").unwrap()]);
}
