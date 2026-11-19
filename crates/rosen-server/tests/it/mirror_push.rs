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
    mirror::owner::mark(data.path()).unwrap();
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
    // #401: the refusal names the commit another writer pushed, before git
    // would answer with a bare `[rejected] (fetch first)`.
    wait_for("mirror refuses the divergent land", || {
        state.events.after(0).iter().any(|event| {
            event.kind == EventKind::Mirror
                && event.detail["status"] == "failed"
                && event.detail["rejected"] == true
                && event.detail["error"]
                    .as_str()
                    .is_some_and(|error| error.contains(&format!("  {outside} outside")))
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

/// Commits `contents` to a.txt in `source` on top of the mirror's main and
/// pushes it, as another Rosen server's land reaches the shared mirror.
fn land_elsewhere(source: &Path, mirror: &Path, contents: &[&str]) -> ObjectId {
    let remote = mirror.to_str().unwrap();
    git(source, &["fetch", "--quiet", remote, "main"]);
    git(source, &["checkout", "--quiet", "-B", "main", "FETCH_HEAD"]);
    for text in contents {
        std::fs::write(source.join("a.txt"), text).unwrap();
        git(source, &["commit", "--quiet", "-am", text.trim()]);
    }
    git(source, &["push", "--quiet", remote, "main"]);
    git(mirror, &["rev-parse", "refs/heads/main"])
        .parse()
        .unwrap()
}

/// A server that imported a one-commit repository with a bare mirror. The
/// directories live as long as the server.
struct Mirrored {
    source: tempfile::TempDir,
    _remote: tempfile::TempDir,
    mirror: std::path::PathBuf,
    _data: tempfile::TempDir,
    state: Arc<ServerState>,
    runtime: tokio::runtime::Runtime,
}

fn mirrored_server() -> Mirrored {
    let source = tempfile::tempdir().unwrap();
    git(source.path(), &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(
        source.path().join("a.txt"),
        "one
",
    )
    .unwrap();
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "--quiet", "-m", "first"]);
    let remote = tempfile::tempdir().unwrap();
    let mirror = remote.path().join("mirror.git");
    let mirror_arg = mirror.to_string_lossy().into_owned();
    let source_arg = source.path().to_string_lossy().into_owned();
    git(
        remote.path(),
        &["clone", "--quiet", "--bare", &source_arg, &mirror_arg],
    );
    let data = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.path().to_path_buf(),
        endpoint: Endpoint::new("rosen-mirror-catch-up-test-unused"),
        warm_pool_dir: None,
    });
    mirror::owner::mark(data.path()).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let imported = service::handle(&state, source_arg, None, Some(mirror_arg));
    assert!(matches!(runtime.block_on(imported), Outcome::Ok(_)));
    Mirrored {
        source,
        _remote: remote,
        mirror,
        _data: data,
        state,
        runtime,
    }
}

#[test]
fn catch_up_fast_forwards_to_another_servers_lands() {
    // Risk (#416): a server whose mirror gained another server's lands can
    // never push again. Catching up must move the base only by
    // fast-forward, keep the event log the clients follow, rebase open
    // changes, and leave the next land a fast-forward push.
    let Mirrored {
        source,
        mirror: mirror_dir,
        state,
        runtime,
        // Bound, not `..`: an unbound temporary directory is deleted here.
        _remote: _remote_dir,
        _data: _data_dir,
    } = mirrored_server();
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    let (_, imported_base) = ledger.project_base().unwrap().unwrap();
    let change = open_change(&state, imported_base);

    let elsewhere = land_elsewhere(source.path(), &mirror_dir, &["two\n", "three\n"]);
    let seen = state.events.announced_seq();
    let catch_up = || runtime.block_on(rosen_server::rpc::dispatch(&state, Request::MirrorCatchUp));
    let Outcome::Ok(Response::MirrorCaughtUp(answer)) = catch_up() else {
        panic!("expected the catch-up to succeed");
    };
    assert_eq!(answer.status, rosen_protocol::CatchUpStatus::FastForwarded);
    assert_eq!(
        (answer.old_base, answer.new_base),
        (imported_base, elsewhere)
    );
    assert_eq!(answer.commits.len(), 2);
    assert_eq!(answer.rebased, vec![change.to_string()]);
    assert_eq!(ledger.base("main").unwrap(), Some(elsewhere));
    let events = state.events.after(seen);
    assert!(events.first().is_some_and(|event| event.seq == seen + 1));
    assert!(events
        .iter()
        .any(|event| event.kind == EventKind::Mirror && event.detail["status"] == "caught_up"));
    assert!(events.iter().any(|event| event.kind == EventKind::Rebased
        && event.change.as_deref() == Some(&change.to_string())));
    assert_rebased_head(&state, change);

    let again = catch_up();
    assert!(
        matches!(again, Outcome::Ok(Response::MirrorCaughtUp(a)) if a.status == rosen_protocol::CatchUpStatus::UpToDate)
    );
    let worker = runtime.spawn(mirror::run(Arc::clone(&state)));
    let next = land(&state, 7, None);
    let mirror_tip = || git(&mirror_dir, &["rev-parse", "refs/heads/main"]);
    wait_for("the next land pushes fast-forward", || {
        mirror_tip() == next.to_hex()
    });
    refuse_diverged(&state, &runtime, source.path(), &mirror_dir);
    state.request_shutdown();
    runtime.block_on(worker).unwrap();
}

/// An open change on the imported base that adds b.txt.
fn open_change(state: &ServerState, base: ObjectId) -> ChangeId {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let one = store.put_blob(b"one\n").unwrap();
    let mine = store.put_blob(b"mine\n").unwrap();
    let tree = Tree::new(vec![
        TreeEntry::new(FileMode::Regular, b"a.txt".to_vec(), one).unwrap(),
        TreeEntry::new(FileMode::Regular, b"b.txt".to_vec(), mine).unwrap(),
    ])
    .unwrap();
    let head = store.put_tree(&tree).unwrap();
    let change = ChangeId::from_bytes([6; 16]);
    state
        .changes
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO changes (id, owner, parent_base, state, head, created_at, updated_at)
             VALUES (?1, 'agent', ?2, 'open', ?3, 0, 0)",
                rusqlite::params![change.to_string(), base.to_hex(), head.to_hex()],
            )?;
            Ok(())
        })
        .unwrap();
    change
}

/// The rebased head keeps the change's file and takes the mirror's edit.
fn assert_rebased_head(state: &ServerState, change: ChangeId) {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let record = state.changes.show(&change.to_string()).unwrap();
    let tree = store.read_tree(&record.change.head).unwrap();
    let text = |name: &[u8]| {
        let entry = tree.entries().iter().find(|e| e.name == name).unwrap();
        store.read_blob(&entry.id).unwrap().data
    };
    assert_eq!(text(b"a.txt"), b"three\n");
    assert_eq!(text(b"b.txt"), b"mine\n");
}

/// Histories that diverged are refused, naming both tips, and the base
/// stays where it was.
fn refuse_diverged(
    state: &Arc<ServerState>,
    runtime: &tokio::runtime::Runtime,
    source: &Path,
    mirror: &Path,
) {
    let elsewhere = land_elsewhere(source, mirror, &["four\n"]);
    let here = land(state, 8, None);
    let refused = runtime.block_on(rosen_server::rpc::dispatch(state, Request::MirrorCatchUp));
    let Outcome::Error(error) = refused else {
        panic!("expected diverged histories to be refused");
    };
    assert_eq!(error.code, ErrorCode::Refused);
    assert!(error.message.contains("diverged"), "{}", error.message);
    assert!(error.message.contains(&here.to_hex()), "{}", error.message);
    assert!(
        error.message.contains(&elsewhere.to_hex()),
        "{}",
        error.message
    );
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    assert_eq!(ledger.base("main").unwrap(), Some(here));
}

#[test]
fn only_the_mirror_owner_pushes() {
    // Risk (#401): a second server for the same project, with the same
    // rosen.toml and credentials, pushed its own land to the shared mirror,
    // and the authoritative server's lands then stayed local. A server not
    // marked as the mirror owner must land and push nothing.
    let Mirrored {
        mirror: mirror_dir,
        state,
        runtime,
        source: _source,
        _remote: _remote_dir,
        _data: data_dir,
    } = mirrored_server();
    mirror::owner::unmark(data_dir.path()).unwrap();
    let mirror_tip = || git(&mirror_dir, &["rev-parse", "refs/heads/main"]);
    let before = mirror_tip();
    let landed = land(&state, 9, None);
    let synced = runtime.block_on(mirror::sync_once(&state.config)).unwrap();
    assert!(matches!(synced, mirror::Synced::NotOwner { commit, .. } if commit == landed));
    assert_eq!(mirror_tip(), before);

    mirror::owner::mark(data_dir.path()).unwrap();
    let synced = runtime.block_on(mirror::sync_once(&state.config)).unwrap();
    assert!(matches!(synced, mirror::Synced::Pushed { commit, .. } if commit == landed));
    assert_eq!(mirror_tip(), landed.to_hex());
}
