//! End-to-end check of the git mirror (SPEC R1 "Accept when") against real
//! git.
//!
//! Risk: the mirror is the copy of landed history outside Rosen. A push
//! that rewrites its branch loses commits someone else put there, and a
//! push that never follows a land leaves the mirror silently stale.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rosen_core::{
    ChangeId, FileMode, LandedCommit, LandedTrailers, ObjectId, Signature, Tree, TreeEntry,
};
use rosen_protocol::{Endpoint, ErrorCode, EventKind, Outcome, Request, Response};
use rosen_server::landing::{CasResult, LandRecord, Ledger, Tier};
use rosen_server::reviews::approvals;
use rosen_server::{import::service, mirror, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use serde_json::json;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git is installed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "git {args:?}: {stderr}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Lands one change on the base as the land path does: the landed commit
/// goes to the store, the ledger moves the base, and `landed` is published.
fn land(state: &ServerState, n: u8, tree: Option<ObjectId>) -> ObjectId {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    let (name, base) = ledger.project_base().unwrap().unwrap();
    let tree = tree.unwrap_or_else(|| store.read_commit(&base).unwrap().tree);
    let change = ChangeId::from_bytes([n; 16]);
    register_change(state, change, base, tree);
    let landed = LandedCommit {
        base,
        tree,
        author: Signature::new("agent", "agent@rosen", 1_700_000_000 + i64::from(n)),
        committer: Signature::new("rosen-server", "rosen-server@rosen", 1_700_000_000),
        description: format!("change {n}"),
        trailers: LandedTrailers {
            change,
            receipt: None,
            approvers: Vec::new(),
        },
    };
    let commit = store.put_commit(&landed.to_commit()).unwrap();
    let record = LandRecord {
        ref_name: name,
        change,
        head: tree,
        commit,
        tier: Tier::ProseOnly,
    };
    let swapped = ledger.compare_and_swap(&base, &record).unwrap();
    assert!(matches!(swapped, CasResult::Swapped { .. }));
    let change = Some(change.to_string());
    state.events.publish(EventKind::Landed, change, json!({}));
    commit
}

fn register_change(state: &ServerState, change: ChangeId, base: ObjectId, tree: ObjectId) {
    // The CAS closes a real registry entry in its transaction, and needs an
    // approval for its head (R7).
    let approval = rosen_protocol::ApproveRequest {
        change: change.to_string(),
        reviewer: rosen_core::AgentId::new("reviewer").unwrap(),
        head: tree,
    };
    state
        .changes
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO changes (id, owner, parent_base, state, head, created_at, updated_at)
             VALUES (?1, 'agent', ?2, 'ready', ?3, 0, 0)",
                rusqlite::params![change.to_string(), base.to_hex(), tree.to_hex()],
            )?;
            Ok(())
        })
        .unwrap();
    let approve = |conn: &mut rusqlite::Connection| Ok(approvals::approve(conn, &approval));
    state.changes.with_conn(approve).unwrap().unwrap();
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn lands_reach_the_mirror_as_fast_forwards_only() {
    let source = tempfile::tempdir().unwrap();
    git(source.path(), &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(source.path().join("a.txt"), "one\n").unwrap();
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "--quiet", "-m", "first"]);
    let remote = tempfile::tempdir().unwrap();
    let mirror_dir = remote.path().join("mirror.git");
    let mirror_arg = mirror_dir.to_string_lossy().into_owned();
    git(
        remote.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            &source.path().to_string_lossy(),
            &mirror_arg,
        ],
    );

    let data = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.path().to_path_buf(),
        endpoint: Endpoint::new("rosen-mirror-test-unused"),
        warm_pool_dir: None,
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let source_arg = source.path().to_string_lossy().into_owned();
    let imported = service::handle(&state, source_arg, None, Some(mirror_arg.clone()));
    assert!(matches!(runtime.block_on(imported), Outcome::Ok(_)));
    let worker = runtime.spawn(mirror::run(Arc::clone(&state)));

    // Accept when: the mirror's log is the imported history plus one
    // commit per landed change, trailers included.
    let first = land(&state, 1, None);
    let second = land(&state, 2, None);
    let mirror_tip = || git(&mirror_dir, &["rev-parse", "refs/heads/main"]);
    wait_for("both lands on the mirror", || {
        mirror_tip() == second.to_hex()
    });
    let log = git(&mirror_dir, &["log", "--format=%H", "main"]);
    let imported_log = git(source.path(), &["log", "--format=%H", "main"]);
    let expected = format!("{}\n{}\n{imported_log}", second.to_hex(), first.to_hex());
    assert_eq!(log, expected);
    let message = git(&mirror_dir, &["log", "-1", "--format=%B", "main"]);
    assert!(message.contains(&format!("Rosen-Change: {}", ChangeId::from_bytes([2; 16]))));

    recover_direct_push(&state, &runtime, source.path(), &mirror_dir);
    state.request_shutdown();
    runtime.block_on(worker).unwrap();
}

fn recover_direct_push(
    state: &Arc<ServerState>,
    runtime: &tokio::runtime::Runtime,
    source: &Path,
    mirror: &Path,
) {
    // R1: a direct push must survive recovery, and its edits must go through
    // normal landing before the operator can adopt its history.
    let remote = mirror.to_str().unwrap();
    let mirror_tip = || git(mirror, &["rev-parse", "refs/heads/main"]);
    git(source, &["fetch", "--quiet", remote, "main"]);
    git(source, &["checkout", "--quiet", "-B", "main", "FETCH_HEAD"]);
    std::fs::write(source.join("a.txt"), "outside\n").unwrap();
    git(source, &["commit", "--quiet", "-am", "outside"]);
    git(source, &["push", "--quiet", remote, "main"]);
    let outside: ObjectId = mirror_tip().parse().unwrap();
    let before_port = land(state, 3, None);
    wait_for("mirror rejects the divergent land", || {
        state.events.after(0).iter().any(|event| {
            event.kind == EventKind::Mirror
                && event.detail["status"] == "failed"
                && event.detail["rejected"] == true
        })
    });
    let adopt = |commit| {
        runtime.block_on(rosen_server::rpc::dispatch(
            state,
            Request::MirrorAdopt { commit },
        ))
    };
    assert!(
        matches!(adopt(before_port), Outcome::Error(err) if err.code == ErrorCode::Refused && err.message.contains("expected"))
    );
    assert!(
        matches!(adopt(outside), Outcome::Error(err) if err.code == ErrorCode::Refused && err.message.contains("not present"))
    );
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    assert_eq!(ledger.base("main").unwrap(), Some(before_port));
    assert_eq!(mirror_tip(), outside.to_hex());
    let tree = ported_tree(state);
    let ported = land(state, 4, Some(tree));
    let Outcome::Ok(Response::MirrorAdopted(answer)) = adopt(outside) else {
        panic!("expected mirror adoption to succeed");
    };
    assert_eq!(answer.old_base, ported);
    assert_eq!(ledger.base("main").unwrap(), Some(answer.new_base));
    wait_for("mirror fast-forwards after adoption", || {
        mirror_tip() == answer.new_base.to_hex()
    });
    assert_eq!(
        git(mirror, &["log", "-1", "--format=%P"]),
        format!("{ported} {outside}")
    );
    assert_eq!(git(mirror, &["rev-parse", "main^{tree}"]), tree.to_hex());
    let next = land(state, 5, None);
    wait_for("later land reaches the mirror", || {
        mirror_tip() == next.to_hex()
    });
}

fn ported_tree(state: &ServerState) -> ObjectId {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let outside = store.put_blob(b"outside\n").unwrap();
    let rosen = store.put_blob(b"Rosen-only change\n").unwrap();
    let tree = Tree::new(vec![
        TreeEntry::new(FileMode::Regular, b"a.txt".to_vec(), outside).unwrap(),
        TreeEntry::new(FileMode::Regular, b"b.txt".to_vec(), rosen).unwrap(),
    ])
    .unwrap();
    store.put_tree(&tree).unwrap()
}
