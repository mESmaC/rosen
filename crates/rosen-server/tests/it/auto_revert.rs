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

#[tokio::test(flavor = "multi_thread")]
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
        warm_pool_dir: None,
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

fn setup_with_config(path: &std::path::Path, toml: &str) -> (Arc<ServerState>, Ledger, ObjectId) {
    let s = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("revert-platform-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(s.config.objects_dir()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let entry = rosen_core::TreeEntry::new(
        rosen_core::FileMode::Regular,
        "rosen.toml",
        store.put_blob(toml.as_bytes()).unwrap(),
    )
    .unwrap();
    let root = store
        .put_commit(&Commit {
            tree: store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap(),
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"base\n".to_vec(),
        })
        .unwrap();
    let ledger = Ledger::open(&s.config.metadata_path()).unwrap();
    ledger.init_base("main", &root).unwrap();
    (s, ledger, root)
}

fn check_kind(cell: &str) -> CheckKind {
    match cell {
        "build" => CheckKind::Build,
        "test" => CheckKind::Test,
        other => panic!("bad check kind {other}"),
    }
}

fn platform_toml(review: &str, required: &[&str], advisory: &[&str]) -> String {
    let req = required
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!(
        "review = \"{review}\"\n\
         [checks.pre_land]\nplatforms = [{req}]\ncommands = [\"build\"]\n\
         [checks.pre_land.platform_commands]\n"
    );
    for plat in advisory {
        out.push_str(&format!("{plat} = [\"advisory-build\"]\n"));
    }
    out.push_str(&format!(
        "[checks.post_land]\nplatforms = [{req}]\ncommands = [\"test\"]\n\
         [checks.post_land.platform_commands]\n"
    ));
    for plat in advisory {
        out.push_str(&format!("{plat} = [\"advisory-test\"]\n"));
    }
    out
}

// Risk (R12, #440, #461): review-mode/advisory disposition and culprit-keyed
// shared inverse reuse across platforms must match the proved Lean model.
#[tokio::test(flavor = "multi_thread")]
async fn mode_and_platform_reverts_match_lean_fixtures() {
    let fixture = include_str!("../../../../proofs/fixtures/auto-revert.tsv");
    let (mut mode_rows, mut platform_rows) = (0usize, 0usize);
    for line in fixture.lines().skip(1) {
        if let Some(row) = line.strip_prefix("mode\t") {
            check_mode_row(row).await;
            mode_rows += 1;
        } else if let Some(row) = line.strip_prefix("platform\t") {
            check_platform_revert_row(row).await;
            platform_rows += 1;
        }
    }
    assert!(mode_rows > 0, "expected mode rows in auto-revert.tsv");
    assert!(
        platform_rows > 0,
        "expected platform rows in auto-revert.tsv"
    );
}

async fn check_mode_row(row: &str) {
    let c: Vec<&str> = row.split('\t').collect();
    assert_eq!(c.len(), 4, "bad mode row: {row}");
    let (review, kind, advisory, expected) = (c[0], check_kind(c[1]), c[2] == "1", c[3] == "1");
    let temp = tempfile::tempdir().unwrap();
    let toml = platform_toml(review, &["windows"], &["linux"]);
    let (state, ledger, root) = setup_with_config(temp.path(), &toml);
    let (original, bad) = land_file(&state, root, "bad.rs").await;
    let plat = if advisory { "linux" } else { "windows" };
    let culprit = BisectCulprit {
        commit: bad,
        parent: root,
        change: original.id.to_string(),
        output: "failure".into(),
    };
    rosen_server::post_land::failures::apply(&state, 1, kind, Some(plat), &culprit, &[]).unwrap();
    let Response::PostLandFailures(records) = call(
        &state,
        Request::PostLandFailures {
            change: Some(original.id.to_string()),
        },
    )
    .await
    else {
        panic!()
    };
    assert_eq!(records.len(), 1, "{row}");
    assert_eq!(records[0].requires_revert, expected, "{row}");
    assert_eq!(records[0].revert.is_some(), expected, "{row}");
    let base = ledger.base("main").unwrap().unwrap();
    assert_eq!(base != bad, expected, "{row}");
}

fn split_platforms<'a>(entries: &[(&'a str, bool)]) -> (Vec<&'a str>, Vec<&'a str>) {
    let (mut required, mut advisory) = (Vec::new(), Vec::new());
    for &(plat, is_adv) in entries {
        let target = if is_adv { &mut advisory } else { &mut required };
        if !target.contains(&plat) {
            target.push(plat);
        }
    }
    if required.is_empty() {
        required.push("windows");
    }
    (required, advisory)
}

async fn check_platform_revert_row(row: &str) {
    let c: Vec<&str> = row.split('\t').collect();
    assert_eq!(c.len(), 12, "bad platform row: {row}");
    let (k1, p1, a1) = (check_kind(c[1]), c[2], c[3] == "1");
    let (k2, p2, a2) = (check_kind(c[4]), c[5], c[6] == "1");
    let (exp_records, exp_inverses): (usize, i64) = (c[7].parse().unwrap(), c[8].parse().unwrap());
    let (exp_shared, exp_r1, exp_r2) = (c[9] == "1", c[10] == "1", c[11] == "1");
    let (required, advisory) = split_platforms(&[(p1, a1), (p2, a2)]);
    let temp = tempfile::tempdir().unwrap();
    let toml = platform_toml(c[0], &required, &advisory);
    let (state, _ledger, root) = setup_with_config(temp.path(), &toml);
    let (original, bad) = land_file(&state, root, "bad.rs").await;
    let _ = land_file(&state, bad, "later.rs").await;
    for (sess, kind, plat, out) in [(1, k1, p1, "fail1"), (2, k2, p2, "fail2")] {
        let culprit = BisectCulprit {
            commit: bad,
            parent: root,
            change: original.id.to_string(),
            output: out.into(),
        };
        rosen_server::post_land::failures::apply(&state, sess, kind, Some(plat), &culprit, &[])
            .unwrap();
    }
    let Response::PostLandFailures(records) = call(
        &state,
        Request::PostLandFailures {
            change: Some(original.id.to_string()),
        },
    )
    .await
    else {
        panic!()
    };
    assert_eq!(records.len(), exp_records, "{row}");
    let conn = rusqlite::Connection::open(state.config.metadata_path()).unwrap();
    let inv_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_reverts WHERE culprit=?1",
            [bad.to_hex()],
            |r| r.get(0),
        )
        .unwrap_or(0);
    assert_eq!(inv_count, exp_inverses, "{row}");
    let r1 = records
        .iter()
        .find(|r| r.kind == k1 && r.platform.as_deref() == Some(p1))
        .expect("record 1");
    let r2 = records
        .iter()
        .find(|r| r.kind == k2 && r.platform.as_deref() == Some(p2))
        .expect("record 2");
    assert_eq!(
        (r1.revert.is_some(), r2.revert.is_some()),
        (exp_r1, exp_r2),
        "{row}"
    );
    assert_eq!(
        r1.revert.is_some() && r1.revert == r2.revert,
        exp_shared,
        "{row}"
    );
}
