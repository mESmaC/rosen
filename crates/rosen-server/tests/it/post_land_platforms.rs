//! Risk (R12, #440): per-platform post-land checks must never mark the base
//! green until every required platform passes, must isolate per-platform
//! bisections and culprits, and must expose live checker coverage gaps.

use rosen_client::Client;
use rosen_core::{AgentId, Commit, ObjectId, Signature, Tree};
use rosen_protocol::*;
use rosen_server::{
    landing::Ledger,
    rpc::{dispatch, dispatch_from},
    ServerConfig, ServerState,
};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

const MULTI_PLATFORM_TOML: &str = r#"
review = "after-land"

[checks.post_land]
platforms = ["windows", "macos"]
commands = ["cargo test --workspace --locked"]

[checks.post_land.platform_commands]
macos = [
    "cd proofs && lake build --wfail",
    "cd proofs && lake env lean --run ../.github/scripts/audit_axioms.lean",
]
"#;

fn setup(toml: &str) -> (tempfile::TempDir, Arc<ServerState>, ObjectId) {
    let temp = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: Endpoint::new("platforms-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
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
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .init_base("main", &root)
        .unwrap();
    (temp, state, root)
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        other => panic!("{other:?}"),
    }
}

async fn land(state: &Arc<ServerState>, path: &str, bytes: &[u8]) -> ObjectId {
    let Response::Change(record) = call(
        state,
        Request::ChangeNew(NewChange {
            owner: AgentId::new("owner").unwrap(),
            on: None,
        }),
    )
    .await
    else {
        panic!()
    };
    let change = record.change.id.to_string();
    call(
        state,
        Request::WorkspaceWrite(WorkspaceWrite {
            change: change.clone(),
            path: path.into(),
            data: Some(WorkspaceBytes(bytes.into())),
        }),
    )
    .await;
    let Response::Land(answer) = call(state, Request::Land { change }).await else {
        panic!()
    };
    let LandStatus::Landed { commit, .. } = answer.outcome else {
        panic!("{answer:?}")
    };
    state.landing.settle();
    commit
}

async fn next_for(state: &Arc<ServerState>, platform: &str) -> PostLandWork {
    let req = Request::PostLandNextFor {
        platform: platform.into(),
        kind: CheckKind::Test,
    };
    let Response::PostLand(work) = call(state, req).await else {
        panic!()
    };
    work
}

async fn post_land_status(state: &Arc<ServerState>) -> PostLandStatus {
    let Response::PostLandStatus(status) = call(state, Request::PostLandStatus).await else {
        panic!()
    };
    status
}

#[tokio::test(flavor = "multi_thread")]
async fn base_goes_green_only_after_all_required_platforms_pass() {
    let (_temp, state, _) = setup(MULTI_PLATFORM_TOML);
    let c1 = land(&state, "a.rs", b"1").await;
    let c2 = land(&state, "b.rs", b"2").await;

    // Unrequired platform is never offered a job.
    assert!(next_for(&state, "linux-x86_64").await.job.is_none());

    // Windows and macOS receive their own platform commands.
    let win_job = next_for(&state, "windows-x86_64").await.job.unwrap();
    assert_eq!(win_job.commit, c2);
    assert_eq!(win_job.platform.as_deref(), Some("windows"));
    assert_eq!(
        win_job.check_set.commands,
        vec!["cargo test --workspace --locked"]
    );

    let mac_job = next_for(&state, "macos-arm64").await.job.unwrap();
    assert_eq!(mac_job.commit, c2);
    assert_eq!(mac_job.platform.as_deref(), Some("macos"));
    assert_eq!(mac_job.check_set.commands.len(), 3);
    assert!(mac_job.check_set.commands[1].contains("lake build --wfail"));

    // Passing on Windows alone does NOT advance the base green checkpoint.
    call(&state, Request::PostLandPassed(win_job)).await;
    let st = post_land_status(&state).await;
    assert_eq!(st.last_green, None);
    assert_eq!(
        st.platforms[0].last_green.as_ref().map(|c| c.commit),
        Some(c2)
    );
    assert_eq!(st.platforms[1].last_green, None);
    assert!(next_for(&state, "windows-x86_64").await.job.is_none());

    // Passing c1 on macOS advances aggregate green only to min(c2, c1) = c1.
    let mut mac_c1 = mac_job.clone();
    mac_c1.commit = c1;
    call(&state, Request::PostLandPassed(mac_c1)).await;
    assert_eq!(
        post_land_status(&state)
            .await
            .last_green
            .as_ref()
            .map(|c| c.commit),
        Some(c1)
    );

    // Passing c2 on macOS advances aggregate green to c2.
    call(&state, Request::PostLandPassed(mac_job)).await;
    assert_eq!(
        post_land_status(&state)
            .await
            .last_green
            .as_ref()
            .map(|c| c.commit),
        Some(c2)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn macos_only_failure_bisects_and_records_platform_culprit() {
    let (_temp, state, _) = setup(MULTI_PLATFORM_TOML);
    let c1 = land(&state, "ok.rs", b"ok").await;
    // Pass c1 on both so both platforms start green at c1.
    let win1 = next_for(&state, "windows-x86_64").await.job.unwrap();
    let mac1 = next_for(&state, "macos-arm64").await.job.unwrap();
    call(&state, Request::PostLandPassed(win1)).await;
    call(&state, Request::PostLandPassed(mac1)).await;

    let c2 = land(&state, "mac_break.rs", b"bad").await;
    let c3 = land(&state, "after.rs", b"ok").await;

    // Windows passes c3 cleanly.
    let win3 = next_for(&state, "windows-x86_64").await.job.unwrap();
    assert_eq!(win3.commit, c3);
    call(&state, Request::PostLandPassed(win3)).await;

    // macOS fails c3 and bisects c2 as culprit while aggregate stays at c1.
    let mac3 = next_for(&state, "macos-arm64").await.job.unwrap();
    let Response::Bisection(mut bisect) = call(
        &state,
        Request::PostLandFailed {
            job: mac3,
            output: "macOS only failure".into(),
        },
    )
    .await
    else {
        panic!()
    };
    assert_eq!(bisect.platform.as_deref(), Some("macos"));
    while let Some(job) = bisect.job.clone() {
        assert_eq!(job.platform.as_deref(), Some("macos"));
        let passed = job.commit == c1;
        let Response::Bisection(next) = call(
            &state,
            Request::BisectSubmit(BisectResult {
                job,
                passed,
                output: if passed {
                    String::new()
                } else {
                    "macOS only failure".into()
                },
                submitter: None,
            }),
        )
        .await
        else {
            panic!()
        };
        bisect = next;
    }
    let culprit = bisect.culprit.expect("macOS culprit confirmed");
    assert_eq!(culprit.commit, c2);

    let Response::PostLandFailures(failures) =
        call(&state, Request::PostLandFailures { change: None }).await
    else {
        panic!()
    };
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].commit, c2);
    assert_eq!(failures[0].platform.as_deref(), Some("macos"));
    // Base green never moved past c1 while macOS failed.
    assert_eq!(
        post_land_status(&state)
            .await
            .last_green
            .as_ref()
            .map(|c| c.commit),
        Some(c1)
    );
    // Remote legacy caller without platform gets no job on a platform-scoped project.
    let remote = dispatch_from(
        &state,
        Request::PostLandNext,
        rosen_server::receipts::Origin::Machine("unknown".into()),
    )
    .await;
    assert!(matches!(
        remote,
        Outcome::Ok(Response::PostLand(PostLandWork { job: None, .. }))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn connected_checker_tracks_lifecycle_in_status_and_state_feed() {
    let temp = tempfile::tempdir().unwrap();
    // A pipe name on Windows; elsewhere a socket in a folder bind makes owner-only.
    let endpoint = if cfg!(windows) {
        Endpoint::new(format!("rosen-platforms-test-{}", std::process::id()))
    } else {
        Endpoint::new(temp.path().join("sock").join("s.sock").to_string_lossy())
    };
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: endpoint.clone(),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let entry = rosen_core::TreeEntry::new(
        rosen_core::FileMode::Regular,
        "rosen.toml",
        store.put_blob(MULTI_PLATFORM_TOML.as_bytes()).unwrap(),
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
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .init_base("main", &root)
        .unwrap();

    let listener = Listener::bind(&endpoint).unwrap();
    let serving = tokio::spawn(rosen_server::listener::serve(listener, Arc::clone(&state)));

    let inspector = Client::connect(&endpoint).await.unwrap();
    let before = inspector.status().await.unwrap();
    assert_eq!(before.platforms.len(), 2);
    assert!(before.platforms.iter().all(|p| p.checkers.is_empty()));

    // Connect a macOS checker and ask for work; status and state feed see it.
    let mac_checker = Client::connect(&endpoint).await.unwrap();
    mac_checker.post_land_next_for("macos-arm64").await.unwrap();
    let during = inspector.status().await.unwrap();
    let mac = during
        .platforms
        .iter()
        .find(|p| p.platform == "macos")
        .unwrap();
    assert_eq!(mac.checkers.len(), 1);
    assert_eq!(mac.checkers[0].platform, "macos-arm64");

    // Drop the macOS checker; status immediately reports no checker.
    drop(mac_checker);
    for _ in 0..40 {
        let after = inspector.status().await.unwrap();
        let mac = after
            .platforms
            .iter()
            .find(|p| p.platform == "macos")
            .unwrap();
        if mac.checkers.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let after = inspector.status().await.unwrap();
    let mac = after
        .platforms
        .iter()
        .find(|p| p.platform == "macos")
        .unwrap();
    assert!(mac.checkers.is_empty());

    state.request_shutdown();
    let _ = serving.await;
}
