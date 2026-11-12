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

use rosen_core::{ChangeId, LandedCommit, LandedTrailers, ObjectId, Signature};
use rosen_protocol::{Endpoint, EventKind, Outcome};
use rosen_server::landing::{CasResult, LandRecord, Ledger, Tier};
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
fn land(state: &ServerState, n: u8) -> ObjectId {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    let (name, base) = ledger.project_base().unwrap().unwrap();
    let tree = store.read_commit(&base).unwrap().tree;
    let change = ChangeId::from_bytes([n; 16]);
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
        head: commit,
        commit,
        tier: Tier::ProseOnly,
    };
    let swapped = ledger.compare_and_swap(&base, &record).unwrap();
    assert!(matches!(swapped, CasResult::Swapped { .. }));
    let change = Some(change.to_string());
    state.events.publish(EventKind::Landed, change, json!({}));
    commit
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
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let source_arg = source.path().to_string_lossy().into_owned();
    let imported = service::handle(&state, source_arg, None, Some(mirror_arg.clone()));
    assert!(matches!(runtime.block_on(imported), Outcome::Ok(_)));
    let worker = runtime.spawn(mirror::run(Arc::clone(&state)));

    // Accept when: the mirror's log is the imported history plus one
    // commit per landed change, trailers included.
    let first = land(&state, 1);
    let second = land(&state, 2);
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

    // Someone else moves the mirror's branch. The next land is not a
    // fast-forward of it, so the push must be refused, not forced, and
    // reported as a mirror event.
    git(
        source.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "outside"],
    );
    git(
        source.path(),
        &["push", "--quiet", "--force", &mirror_arg, "main"],
    );
    let outside = git(source.path(), &["rev-parse", "main"]);
    land(&state, 3);
    wait_for("a mirror failure event", || {
        state.events.after(0).iter().any(|event| {
            event.kind == EventKind::Mirror
                && event.detail["status"] == "failed"
                && event.detail["rejected"] == true
        })
    });
    assert_eq!(mirror_tip(), outside);

    state.request_shutdown();
    runtime.block_on(worker).unwrap();
}
