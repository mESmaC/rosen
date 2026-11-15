//! Risk (R8): updating or landing a parent must preserve every descendant's
//! authored work and commits, while keeping commits in bottom-first order.
use rosen_core::{AgentId, ChangeParent, Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
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

// Fixture codes match StackedChanges.Export's two whole-file tree model.
fn fixture_tree(store: &dyn ObjectStore, code: u8) -> ObjectId {
    let mut files = Vec::new();
    let shared = ["", "original\n", "parent\n", "child\n"][usize::from(code % 4)];
    if !shared.is_empty() {
        files.push(("shared", shared));
    }
    if code / 4 == 1 {
        files.push(("unrelated", "keep\n"));
    }
    let entries = files
        .into_iter()
        .map(|(path, data)| {
            TreeEntry::new(
                FileMode::Regular,
                path,
                store.put_blob(data.as_bytes()).unwrap(),
            )
            .unwrap()
        })
        .collect();
    store.put_tree(&Tree::new(entries).unwrap()).unwrap()
}

async fn fixture_stack(state: &Arc<ServerState>, codes: &[u8]) -> String {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let root = create(state, None).await;
    state
        .changes
        .with_conn(|conn| {
            rosen_server::changes::update(conn, root.parse().unwrap(), |c| {
                c.head = fixture_tree(&store, codes[0]);
                Ok(())
            })
        })
        .unwrap();
    land_parent(state, &root).await;
    let base = Ledger::open(&state.config.metadata_path())
        .unwrap()
        .base("main")
        .unwrap()
        .unwrap();
    let mut basis = store.read_commit(&base).unwrap();
    basis.tree = fixture_tree(&store, codes[1]);
    let basis = store.put_commit(&basis).unwrap();
    let parent = create(state, None).await;
    state
        .changes
        .with_conn(|conn| {
            rosen_server::changes::update(conn, parent.parse().unwrap(), |c| {
                c.parent = ChangeParent::Base(basis);
                c.head = fixture_tree(&store, codes[2]);
                Ok(())
            })
        })
        .unwrap();
    let child = create(state, Some(&parent)).await;
    // Restore the saved inputs directly: a child can lag its parent after
    // propagation conflicts (#146), or before restart recovery runs.
    state
        .changes
        .with_conn(|conn| {
            conn.execute(
                "UPDATE stack_bases SET tree = ?1 WHERE change_id = ?2",
                [fixture_tree(&store, codes[3]).to_hex(), child.clone()],
            )?;
            rosen_server::changes::update(conn, child.parse().unwrap(), |c| {
                c.head = fixture_tree(&store, codes[4]);
                Ok(())
            })
        })
        .unwrap();
    child
}

#[tokio::test]
async fn stack_merge_result_matches_proved_fixture() {
    // Risk (R8): using the stack root as a stale child's basis can receipt a
    // clean tree that silently deletes intervening lands (#146, #182).
    let mut checked = 0;
    for row in include_str!("../../../proofs/fixtures/stacked-changes.tsv")
        .lines()
        .filter(|row| row.starts_with("merge\t"))
    {
        let f: Vec<_> = row.split('\t').collect();
        assert_eq!(f.len(), 8, "{row}");
        let codes: Vec<u8> = f[2..7].iter().map(|s| s.parse().unwrap()).collect();
        let (_temp, state) = setup();
        let child = fixture_stack(&state, &codes).await;
        let Response::MergeResult(answer) =
            call(&state, Request::MergeResult { change: child }).await
        else {
            panic!("{row}")
        };
        if f[7] == "-" {
            assert_eq!(answer.tree, None, "{row}");
            assert_eq!(answer.conflicts, ["shared"], "{row}");
        } else {
            let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
            assert_eq!(
                answer.tree,
                Some(fixture_tree(&store, f[7].parse().unwrap())),
                "{row}"
            );
            assert!(answer.conflicts.is_empty(), "{row}");
        }
        checked += 1;
    }
    assert_eq!(checked, 6);
}

#[tokio::test]
async fn legacy_stack_recovers_only_from_a_recorded_parent_tree() {
    // Risk (#166): an edited stack from before parent trees were recorded
    // must never be merged from a tree the server guessed, and recovering it
    // must keep every authored snapshot.
    let (_temp, state) = setup();
    let parent = create(&state, None).await;
    write(&state, &parent, "shared", "original\n").await;
    let child = create(&state, Some(&parent)).await;
    write(&state, &child, "child", "mine\n").await;
    state
        .changes
        .with_conn(|conn| Ok(conn.execute("DELETE FROM stack_bases", [])?))
        .unwrap();
    write(&state, &parent, "later", "parent\n").await;
    let before = state.changes.show(&child).unwrap().change;
    let recover = |basis| Request::ChangeRecoverBasis {
        change: child.clone(),
        basis,
    };
    // A tree outside the parent's recorded history is refused.
    assert!(matches!(
        dispatch(&state, recover(before.head)).await,
        Outcome::Error(_)
    ));
    let started = state.changes.show(&parent).unwrap().change.snapshots[0].commit;
    let Response::Change(after) = call(&state, recover(started)).await else {
        panic!()
    };
    assert_eq!(after.change.snapshots, before.snapshots);
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let tree = store.read_tree(&after.change.head).unwrap();
    for path in ["shared", "child", "later"] {
        assert!(tree.get(path.as_bytes()).is_some());
    }
    assert!(matches!(
        dispatch(&state, recover(started)).await,
        Outcome::Error(_)
    ));
}

#[tokio::test]
async fn legacy_recovery_matches_fixture_and_preserves_parent_updates() {
    // Risk (R8, #361): replacing an unedited child's implicit basis with a
    // newer parent snapshot silently treats intervening parent work as deleted.
    let mut checked = 0;
    for row in include_str!("../../../proofs/fixtures/stacked-changes.tsv").lines() {
        let fields: Vec<_> = row.split('\t').collect();
        if fields[0] == "recovery" {
            check_legacy_recovery(fields[1] == "1", fields[2] == "1", fields[3] == "1").await;
            checked += 1;
        }
    }
    assert_eq!(checked, 4);
}

async fn check_legacy_recovery(edited: bool, recorded: bool, allowed: bool) {
    let (_temp, state) = setup();
    let parent = create(&state, None).await;
    write(&state, &parent, "shared", "original\n").await;
    let child = create(&state, Some(&parent)).await;
    if edited {
        write(&state, &child, "child", "mine\n").await;
    }
    if !recorded {
        state
            .changes
            .with_conn(|conn| {
                Ok(conn.execute("DELETE FROM stack_bases WHERE change_id = ?1", [&child])?)
            })
            .unwrap();
    }
    write(&state, &parent, "later", "parent\n").await;
    let before = state.changes.show(&child).unwrap().change;
    let snapshots = state.changes.show(&parent).unwrap().change.snapshots;
    try_legacy_recovery(&state, &child, &snapshots, allowed).await;
    if !allowed {
        assert_eq!(state.changes.show(&child).unwrap().change, before);
        assert_recorded_basis_count(&state, &child, usize::from(recorded));
    }
    let Response::MergeResult(merged) = call(
        &state,
        Request::MergeResult {
            change: child.clone(),
        },
    )
    .await
    else {
        panic!()
    };
    land_parent(&state, &parent).await;
    let after = state.changes.show(&child).unwrap().change;
    assert_eq!(after.snapshots, before.snapshots);
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    for head in [merged.tree.unwrap(), after.head] {
        let tree = store.read_tree(&head).unwrap();
        let later = tree.get(b"later").expect("parent update must survive");
        assert_eq!(store.read_blob(&later.id).unwrap().data, b"parent\n");
    }
}

async fn try_legacy_recovery(
    state: &Arc<ServerState>,
    child: &str,
    snapshots: &[rosen_core::Snapshot],
    allowed: bool,
) {
    let recover = |basis| Request::ChangeRecoverBasis {
        change: child.into(),
        basis,
    };
    if allowed {
        call(state, recover(snapshots[0].commit)).await;
    } else {
        for snapshot in snapshots {
            for basis in [snapshot.commit, snapshot.tree] {
                let outcome = dispatch(state, recover(basis)).await;
                assert!(matches!(outcome, Outcome::Error(ref e)
                    if e.code == rosen_protocol::ErrorCode::Refused));
            }
        }
    }
}

fn assert_recorded_basis_count(state: &ServerState, child: &str, expected: usize) {
    state
        .changes
        .with_conn(|conn| {
            let count: usize = conn.query_row(
                "SELECT COUNT(*) FROM stack_bases WHERE change_id = ?1",
                [child],
                |r| r.get(0),
            )?;
            assert_eq!(count, expected);
            Ok(())
        })
        .unwrap();
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
            signature: None,
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
    // A land answers before its post-land work runs (issue #190).
    state.landing.settle();
    assert!(matches!(
        answer.outcome,
        rosen_protocol::LandStatus::Landed { .. }
    ));
}
