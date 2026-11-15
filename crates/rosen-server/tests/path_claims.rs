//! SPEC R6 risk: notices must not serialize authors, but exclusive claims
//! must remain atomic; concurrent work must reach both owners, including lists.
use rosen_core::{AgentId, ChangeId, Commit, FileMode, Signature, Tree, TreeEntry};
use rosen_protocol::{
    ClaimOutcome, ClaimRequest, Endpoint, EventKind, NewChange, Outcome, OverlapRecord, Request,
    Response, WorkspaceBytes, WorkspaceWrite,
};
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

fn setup(config: &str) -> (tempfile::TempDir, Arc<ServerState>, [ChangeId; 3]) {
    let dir = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: dir.path().into(),
        endpoint: Endpoint::new("path-notices"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let blob = store.put_blob(config.as_bytes()).unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "rosen.toml", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let base = store
        .put_commit(&Commit {
            tree,
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"base\n".to_vec(),
        })
        .unwrap();
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .init_base("main", &base)
        .unwrap();
    let ids = ["first", "second", "third"].map(|nick| {
        state
            .changes
            .create(&NewChange {
                owner: AgentId::new(nick).unwrap(),
                on: None,
            })
            .unwrap()
            .change
            .id
    });
    (dir, state, ids)
}

fn req(id: ChangeId, path: &str, exclusive: bool) -> ClaimRequest {
    ClaimRequest {
        change: id.to_string(),
        paths: vec![path.into()],
        exclusive,
    }
}

async fn claim(
    state: &Arc<ServerState>,
    id: ChangeId,
    path: &str,
    exclusive: bool,
) -> ClaimOutcome {
    match dispatch(state, Request::ClaimPaths(req(id, path, exclusive))).await {
        Outcome::Ok(Response::Claimed(answer)) => answer,
        other => panic!("claim failed: {other:?}"),
    }
}

fn assert_notice_delivery(state: &ServerState, ids: [ChangeId; 2], replies: [ClaimOutcome; 2]) {
    let overlaps = replies.map(|answer| match answer {
        ClaimOutcome::Granted { overlaps, .. } => overlaps,
        other => panic!("notice refused: {other:?}"),
    });
    assert_eq!(overlaps.iter().map(Vec::len).sum::<usize>(), 1);
    let (earlier, later, reported, owner) = if overlaps[0].is_empty() {
        (ids[0], ids[1], &overlaps[1], "first")
    } else {
        (ids[1], ids[0], &overlaps[0], "second")
    };
    let expected = vec![OverlapRecord {
        change: earlier,
        owner: AgentId::new(owner).unwrap(),
        paths: vec!["SPEC.md".into()],
    }];
    assert_eq!(*reported, expected);
    // R6: both owners learn of concurrent work from the later claim's event.
    // The earlier claimant is named in its overlaps, not as its subject.
    let detail = serde_json::json!({
        "paths": ["SPEC.md"], "exclusive": false, "overlaps": expected,
    });
    for owner in ["first", "second"] {
        let events = state
            .events
            .page(0, Some(&AgentId::new(owner).unwrap()), 100)
            .unwrap()
            .events;
        assert!(
            events.iter().any(|e| e.kind == EventKind::Claim
                && e.change.as_deref() == Some(&later.to_string())
                && e.detail == detail),
            "{owner} did not receive the notice naming both claimants: {events:?}"
        );
    }
}

#[tokio::test]
async fn notices_report_both_owners_and_exclusive_races_have_one_winner() {
    let (_dir, state, [a, b, waiter]) = setup("");
    let peer = ServerState::new(state.config.clone());
    let (first, second) = tokio::join!(
        claim(&state, a, "SPEC.md", false),
        claim(&peer, b, "SPEC.md", false)
    );
    assert_notice_delivery(&state, [a, b], [first, second]);
    let (first, second) = tokio::join!(
        claim(&state, a, "schema.sql", true),
        claim(&peer, b, "schema.sql", true)
    );
    assert_eq!(
        [first, second]
            .iter()
            .filter(|r| matches!(r, ClaimOutcome::Granted { .. }))
            .count(),
        1
    );
    // Notices remain available even when an exclusive claim exists.
    assert!(matches!(
        claim(&state, waiter, "schema.sql", false).await,
        ClaimOutcome::Granted { .. }
    ));
    let waited = dispatch(&state, Request::WaitPaths(req(waiter, "SPEC.md", false))).await;
    assert!(matches!(waited, Outcome::Ok(Response::Waiting(w)) if w.waiting_on.len() == 2));
    for (id, wakes) in [(a, 0), (b, 1)] {
        let released = dispatch(&state, Request::ReleasePaths(req(id, "SPEC.md", false))).await;
        assert!(matches!(released, Outcome::Ok(Response::Released(r)) if r.woken.len() == wakes));
    }
}

#[tokio::test]
async fn notices_include_shares_and_edits_and_exclusive_rejects_shared_files() {
    let (_dir, state, [a, b, c]) =
        setup("generated = [\"out/**\"]\nregistration_lists = [\"Cargo.toml\"]\n");
    dispatch(&state, Request::SharePaths(req(b, "Cargo.toml", false))).await;
    let edited = dispatch(
        &state,
        Request::WorkspaceWrite(WorkspaceWrite {
            change: c.to_string(),
            path: "Cargo.toml".into(),
            data: Some(WorkspaceBytes(b"edited\n".to_vec())),
        }),
    )
    .await;
    assert!(matches!(edited, Outcome::Ok(_)));
    let notice = claim(&state, a, "Cargo.toml", false).await;
    assert!(matches!(notice, ClaimOutcome::Granted { overlaps, .. } if overlaps.len() == 2));
    for path in ["Cargo.toml", "Cargo.*", "out/one", "out/**", "**"] {
        let outcome = dispatch(&state, Request::ClaimPaths(req(a, path, true))).await;
        assert!(matches!(outcome, Outcome::Error(_)), "{path}: {outcome:?}");
    }
    assert!(matches!(
        claim(&state, a, "out/one", false).await,
        ClaimOutcome::Granted { .. }
    ));
    let peer = ServerState::new(state.config.clone());
    let records = rosen_server::claims::live_records(
        &mut rusqlite::Connection::open(peer.config.metadata_path()).unwrap(),
    )
    .unwrap();
    assert!(records.iter().all(|r| !r.exclusive));
    // Recreate the pre-upgrade table: its claims must reopen as locks.
    let conn = rusqlite::Connection::open(state.config.metadata_path()).unwrap();
    conn.execute_batch("ALTER TABLE path_claims DROP COLUMN exclusive;")
        .unwrap();
    rosen_server::claims::ensure_schema(&conn).unwrap();
    drop(conn);
    let upgraded = ServerState::new(state.config.clone());
    upgraded.changes.show(&a.to_string()).unwrap();
    let records = rosen_server::claims::live_records(
        &mut rusqlite::Connection::open(upgraded.config.metadata_path()).unwrap(),
    )
    .unwrap();
    assert!(records
        .iter()
        .filter(|r| r.kind == rosen_protocol::ClaimKind::Claim)
        .all(|r| r.exclusive));
}

#[tokio::test]
async fn claims_match_the_lean_table() {
    let (_dir, state, [a, b, c]) = setup("");
    for row in include_str!("../../../proofs/fixtures/claims.tsv")
        .lines()
        .skip(1)
    {
        let cells: Vec<_> = row.split('\t').collect();
        assert_eq!(cells.len(), 5, "{row}");
        // R6 risk: overlapping notices must block exclusives just like locks do.
        for (held_exclusive, holders) in [(true, vec![a]), (false, vec![a, c])] {
            for (exclusive, expected) in [(true, cells[3]), (false, cells[4])] {
                for &holder in &holders {
                    assert!(matches!(
                        claim(&state, holder, cells[0], held_exclusive).await,
                        ClaimOutcome::Granted { .. }
                    ));
                }
                let answer = claim(&state, b, cells[1], exclusive).await;
                assert_eq!(
                    matches!(answer, ClaimOutcome::Granted { .. }),
                    expected == "1",
                    "{row}: exclusive={exclusive}"
                );
                if let ClaimOutcome::Granted { overlaps, .. } = answer {
                    assert_eq!(!overlaps.is_empty(), cells[2] == "1", "{row}");
                }
                for id in [a, b, c] {
                    let release = ClaimRequest {
                        change: id.to_string(),
                        paths: vec![],
                        exclusive: false,
                    };
                    assert!(matches!(
                        dispatch(&state, Request::ReleasePaths(release)).await,
                        Outcome::Ok(_)
                    ));
                }
            }
        }
    }
}
