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
                durations: None,
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

const ADVISORY_PLATFORM_TOML: &str = r#"
review = "after-land"

[checks.post_land]
platforms = ["windows"]
commands = ["cargo test --workspace --locked"]

[checks.post_land.platform_commands]
macos = [
    "cd proofs && lake build --wfail",
]
linux = [
    "cd proofs && lake build --wfail",
]
"#;

/// Risk (R12, #457): Windows alone must advance post-land green when macOS and
/// Linux are advisory (`platform_commands` only); advisory checkers still get
/// their jobs, appear once they report, and record failures without blocking green.
#[tokio::test(flavor = "multi_thread")]
async fn advisory_platforms_report_results_and_failures_without_gating_green() {
    let (_temp, state, _) = setup(ADVISORY_PLATFORM_TOML);
    let c1 = land(&state, "a.rs", b"1").await;

    // Advisory platforms are hidden from status until they connect or report.
    let before = post_land_status(&state).await;
    assert_eq!(before.platforms.len(), 1);
    assert_eq!(before.platforms[0].platform, "windows");
    assert!(before.platforms[0].required_test);

    // Passing on Windows alone advances aggregate post-land green.
    let win_job = next_for(&state, "windows-x86_64").await.job.unwrap();
    assert_eq!(win_job.commit, c1);
    assert_eq!(win_job.platform.as_deref(), Some("windows"));
    call(&state, Request::PostLandPassed(win_job)).await;
    let after_win = post_land_status(&state).await;
    assert_eq!(after_win.last_green.as_ref().map(|c| c.commit), Some(c1));

    // Advisory macOS checker receives its platform_commands and records its checkpoint.
    let mac_job = next_for(&state, "macos-arm64").await.job.unwrap();
    assert_eq!(mac_job.commit, c1);
    assert_eq!(mac_job.platform.as_deref(), Some("macos"));
    assert_eq!(mac_job.check_set.commands.len(), 2);
    call(&state, Request::PostLandPassed(mac_job)).await;

    let after_mac = post_land_status(&state).await;
    let mac = after_mac
        .platforms
        .iter()
        .find(|p| p.platform == "macos")
        .expect("advisory macOS shown once reported");
    assert!(!mac.required_test);
    assert!(!mac.required_build);
    assert_eq!(mac.last_green.as_ref().map(|c| c.commit), Some(c1));
    assert_eq!(mac.waiting_no_checker_since, None);

    // Advisory Linux failure is bisected and recorded without blocking Windows green.
    let c2 = land(&state, "b.rs", b"2").await;
    let win2 = next_for(&state, "windows-x86_64").await.job.unwrap();
    call(&state, Request::PostLandPassed(win2)).await;
    assert_eq!(
        post_land_status(&state)
            .await
            .last_green
            .as_ref()
            .map(|c| c.commit),
        Some(c2)
    );
    let linux_job = next_for(&state, "linux-x86_64").await.job.unwrap();
    assert_eq!(linux_job.platform.as_deref(), Some("linux"));
    let Response::Bisection(mut bisect) = call(
        &state,
        Request::PostLandFailed {
            job: linux_job,
            output: "linux proof failed".into(),
        },
    )
    .await
    else {
        panic!()
    };
    while let Some(job) = bisect.job.clone() {
        let passed = job.commit != c2;
        let Response::Bisection(next) = call(
            &state,
            Request::BisectSubmit(BisectResult {
                job,
                passed,
                output: if passed {
                    String::new()
                } else {
                    "linux proof failed".into()
                },
                submitter: None,
                durations: None,
            }),
        )
        .await
        else {
            panic!()
        };
        bisect = next;
    }
    assert_eq!(bisect.culprit.as_ref().map(|c| c.commit), Some(c2));
    let Response::PostLandFailures(failures) =
        call(&state, Request::PostLandFailures { change: None }).await
    else {
        panic!()
    };
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].platform.as_deref(), Some("linux"));
    assert!(!failures[0].requires_revert);
}

/// Risk (R12, #457): a required platform with no checker online for an hour while
/// checkpoints wait on it must be reported loudly in status and the state feed.
#[tokio::test(flavor = "multi_thread")]
async fn required_platform_without_checker_for_an_hour_is_reported_waiting() {
    let (temp, state, _) = setup(MULTI_PLATFORM_TOML);
    let c1 = land(&state, "a.rs", b"1").await;

    // Pass on Windows so only macOS is still waiting on c1.
    let win_job = next_for(&state, "windows-x86_64").await.job.unwrap();
    assert_eq!(win_job.commit, c1);
    call(&state, Request::PostLandPassed(win_job)).await;

    // Immediately after landing (< 1 hour), macOS is not yet reported as waiting.
    let fresh = post_land_status(&state).await;
    let mac_fresh = fresh
        .platforms
        .iter()
        .find(|p| p.platform == "macos")
        .unwrap();
    assert_eq!(mac_fresh.waiting_no_checker_since, None);

    // Backdate the landed event by 2 hours; a freshly started server has not yet
    // observed an hour without a matching checker, so it must stay silent (#470).
    let two_hours_ago = rosen_server::post_land::submitter::now_millis() - 2 * 60 * 60 * 1000;
    {
        let conn = rosen_server::metadata::open(&state.config.metadata_path()).unwrap();
        conn.execute("UPDATE events SET created_at = ?1", [two_hours_ago])
            .unwrap();
    }
    let just_started = post_land_status(&state).await;
    let mac_just_started = just_started
        .platforms
        .iter()
        .find(|p| p.platform == "macos")
        .unwrap();
    assert_eq!(mac_just_started.waiting_no_checker_since, None);

    // Once the server itself has run for 2 hours without a macOS checker,
    // PostLandStatus reports macOS as waiting with the exact loud message.
    state.checkers.set_started_at_ms(two_hours_ago);
    let waited = post_land_status(&state).await;
    let win = waited
        .platforms
        .iter()
        .find(|p| p.platform == "windows")
        .unwrap();
    assert_eq!(win.waiting_no_checker_since, None);

    let mac = waited
        .platforms
        .iter()
        .find(|p| p.platform == "macos")
        .unwrap();
    assert_eq!(mac.waiting_no_checker_since, Some(two_hours_ago));
    let message = mac
        .waiting_no_checker_message()
        .expect("waiting message present");
    assert!(
        message.starts_with("post-land waiting on macOS: no checker online since "),
        "{message}"
    );

    // Collapse platforms state feed carries the same waiting report.
    let feed = rosen_server::state_push::platforms_feed_item(&state)
        .await
        .unwrap()
        .expect("platforms item");
    assert_eq!(feed["waiting"][0].as_str(), Some(message.as_str()));
    let mac_feed = feed["platforms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["platform"] == "macos")
        .unwrap();
    assert_eq!(mac_feed["waiting_no_checker_since"], two_hours_ago);
    assert_eq!(mac_feed["waiting_message"].as_str(), Some(message.as_str()));

    // Connecting a macOS checker clears the waiting alert immediately.
    let guard = state.checkers.register(
        "mac-mini".into(),
        rosen_core::Platform::parse("macos-arm64").unwrap(),
    );
    let online = post_land_status(&state).await;
    let mac_online = online
        .platforms
        .iter()
        .find(|p| p.platform == "macos")
        .unwrap();
    assert_eq!(mac_online.waiting_no_checker_since, None);
    drop(guard);

    // Restarting the server right after disconnect must not re-emit the 1-hour alert (#470).
    let restarted = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: Endpoint::new("platforms-contract-restarted"),
        warm_pool_dir: None,
    });
    let after_restart = post_land_status(&restarted).await;
    let mac_restarted = after_restart
        .platforms
        .iter()
        .find(|p| p.platform == "macos")
        .unwrap();
    assert_eq!(mac_restarted.waiting_no_checker_since, None);
}

const OVERLAPPING_MACOS_TOML: &str = r#"
review = "after-land"

[checks.post_land]
platforms = ["macos", "macos-arm64"]
commands = ["cargo test --workspace --locked"]
"#;

/// Risk (R12, #459): overlapping required targets (`macos` and `macos-arm64`)
/// must both receive work from a single matching `macos-arm64` checker so the
/// generic target is never starved and aggregate green advances.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_macos_targets_advance_both_checkpoints_without_starvation() {
    let (_temp, state, _) = setup(OVERLAPPING_MACOS_TOML);
    let c1 = land(&state, "a.rs", b"1").await;

    let arm_c1 = next_for(&state, "macos-arm64").await.job.unwrap();
    assert_eq!(
        (arm_c1.commit, arm_c1.platform.as_deref()),
        (c1, Some("macos-arm64"))
    );

    let Response::PostLand(after_arm) = call(&state, Request::PostLandPassed(arm_c1)).await else {
        panic!()
    };
    assert_eq!(after_arm.last_green, None);
    assert_eq!(
        after_arm.job.as_ref().and_then(|j| j.platform.as_deref()),
        Some("macos")
    );

    let generic_c1 = next_for(&state, "macos-arm64").await.job.unwrap();
    assert_eq!(
        (generic_c1.commit, generic_c1.platform.as_deref()),
        (c1, Some("macos"))
    );
    call(&state, Request::PostLandPassed(generic_c1)).await;
    assert_eq!(
        post_land_status(&state).await.last_green.map(|c| c.commit),
        Some(c1)
    );
    assert!(next_for(&state, "macos-arm64").await.job.is_none());

    // Passing `macos-arm64` on c2 after c3 lands must offer lagging `macos` next.
    let c2 = land(&state, "b.rs", b"2").await;
    let arm_c2 = next_for(&state, "macos-arm64").await.job.unwrap();
    assert_eq!(arm_c2.platform.as_deref(), Some("macos-arm64"));
    let c3 = land(&state, "c.rs", b"3").await;
    call(&state, Request::PostLandPassed(arm_c2)).await;

    let generic_c3 = next_for(&state, "macos-arm64").await.job.unwrap();
    assert_eq!(
        (generic_c3.commit, generic_c3.platform.as_deref()),
        (c3, Some("macos"))
    );
    call(&state, Request::PostLandPassed(generic_c3)).await;
    assert_eq!(
        post_land_status(&state).await.last_green.map(|c| c.commit),
        Some(c2)
    );
}

/// Risk (R12, #460): a generic required target (`macos`) must expand
/// `platform_commands` against the reported checker (`macos-arm64`) while
/// preserving the generic target's checkpoint and bisection scope.
#[tokio::test(flavor = "multi_thread")]
async fn generic_macos_job_expands_arm64_commands_and_preserves_checkpoint() {
    let toml = "review = \"after-land\"\n[checks.post_land]\nplatforms = [\"macos\"]\n\
                [checks.post_land.platform_commands]\nmacos-arm64 = [\"echo arm check\"]\n";
    let (_temp, state, _) = setup(toml);
    let c1 = land(&state, "a.rs", b"1").await;

    let arm = next_for(&state, "macos-arm64").await.job.unwrap();
    assert_eq!(arm.platform.as_deref(), Some("macos"));
    assert_eq!(arm.checker_platform.as_deref(), Some("macos-arm64"));
    assert_eq!(arm.check_set.commands, vec!["echo arm check"]);
    assert!(next_for(&state, "macos-x86_64")
        .await
        .job
        .unwrap()
        .check_set
        .commands
        .is_empty());

    let mut skipped = arm.clone();
    skipped.check_set.commands.clear();
    assert!(matches!(
        dispatch(&state, Request::PostLandPassed(skipped)).await,
        Outcome::Error(_)
    ));
    let mut bad = arm.clone();
    bad.checker_platform = Some("windows-x86_64".into());
    assert!(matches!(
        dispatch(&state, Request::PostLandPassed(bad)).await,
        Outcome::Error(_)
    ));

    let Response::PostLand(after) = call(&state, Request::PostLandPassed(arm)).await else {
        panic!()
    };
    assert_eq!((after.last_green, after.job), (Some(c1), None));
    let st = post_land_status(&state).await;
    assert_eq!(st.platforms.len(), 1);
    assert_eq!(st.platforms[0].platform, "macos");
    assert_eq!(
        st.platforms[0].last_green.as_ref().map(|c| c.commit),
        Some(c1)
    );

    let c2 = land(&state, "b.rs", b"2").await;
    let arm_c2 = next_for(&state, "macos-arm64").await.job.unwrap();
    assert_eq!((arm_c2.commit, arm_c2.check_set.commands.len()), (c2, 1));
    let req = Request::PostLandFailed {
        job: arm_c2,
        output: "arm fail".into(),
    };
    let Response::Bisection(bisect) = call(&state, req).await else {
        panic!()
    };
    assert_eq!(bisect.platform.as_deref(), Some("macos"));
}

/// Risk (R12, #469): omitting `platforms` while declaring `platform_commands`
/// must run shared checks on the default any-platform stream and still offer,
/// record and bisect separate non-reverting advisory jobs for matching checkers.
#[tokio::test(flavor = "multi_thread")]
async fn advisory_only_platform_commands_run_separate_from_default_stream() {
    let toml = "review = \"after-land\"\n\
                [checks.post_land]\ncommands = [\"shared tests\"]\n\
                [checks.post_land.platform_commands]\nlinux = [\"linux tests\"]\n";
    let (_temp, state, _) = setup(toml);
    let c1 = land(&state, "a.rs", b"1").await;

    let shared = next_for(&state, "linux-x86_64").await.job.unwrap();
    assert_eq!((shared.commit, shared.platform.as_deref()), (c1, None));
    assert_eq!(shared.check_set.commands, vec!["shared tests"]);
    let mut tampered = shared.clone();
    tampered.check_set.commands.push("linux tests".into());
    assert!(matches!(
        dispatch(&state, Request::PostLandPassed(tampered)).await,
        Outcome::Error(_)
    ));

    let Response::PostLand(after_shared) = call(&state, Request::PostLandPassed(shared)).await
    else {
        panic!()
    };
    assert_eq!(after_shared.last_green, Some(c1));
    let linux = after_shared.job.expect("separate Linux advisory job");
    assert_eq!(
        (linux.commit, linux.platform.as_deref()),
        (c1, Some("linux"))
    );
    assert_eq!(
        linux.check_set.commands,
        vec!["shared tests", "linux tests"]
    );
    call(&state, Request::PostLandPassed(linux)).await;

    let st = post_land_status(&state).await;
    assert_eq!(st.last_green.as_ref().map(|c| c.commit), Some(c1));
    assert_eq!(st.platforms.len(), 1);
    assert_eq!(st.platforms[0].platform, "linux");
    assert!(!st.platforms[0].is_required());
    assert_eq!(
        st.platforms[0].last_green.as_ref().map(|c| c.commit),
        Some(c1)
    );
    assert!(next_for(&state, "linux-x86_64").await.job.is_none());

    let c2 = land(&state, "b.rs", b"2").await;
    let win = next_for(&state, "windows-x86_64").await.job.unwrap();
    assert_eq!((win.commit, win.platform.as_deref()), (c2, None));
    call(&state, Request::PostLandPassed(win)).await;
    bisect_advisory_linux_failure(&state, c1, c2).await;
}

async fn bisect_advisory_linux_failure(state: &Arc<ServerState>, c1: ObjectId, c2: ObjectId) {
    let linux_c2 = next_for(state, "linux-x86_64").await.job.unwrap();
    assert_eq!(
        (linux_c2.commit, linux_c2.platform.as_deref()),
        (c2, Some("linux"))
    );
    let fail = Request::PostLandFailed {
        job: linux_c2,
        output: "linux advisory failed".into(),
    };
    let Response::Bisection(mut bisect) = call(state, fail).await else {
        panic!()
    };
    assert_eq!(bisect.platform.as_deref(), Some("linux"));
    while let Some(job) = bisect.job.clone() {
        let passed = job.commit == c1;
        let result = BisectResult {
            job,
            passed,
            output: if passed {
                String::new()
            } else {
                "linux advisory failed".into()
            },
            submitter: None,
            durations: None,
        };
        let Response::Bisection(next) = call(state, Request::BisectSubmit(result)).await else {
            panic!()
        };
        bisect = next;
    }
    assert_eq!(bisect.culprit.as_ref().map(|c| c.commit), Some(c2));
    assert!(next_for(state, "linux-x86_64").await.job.is_none());
    let cfg = rosen_core::ProjectConfig::parse(
        "review = \"before-land\"\n\
         [checks.pre_land.platform_commands]\nlinux = [\"linux build\"]\n\
         [checks.post_land.platform_commands]\nlinux = [\"linux test\"]\n",
    )
    .unwrap();
    assert!(
        !rosen_server::post_land::failures::confirmed_failure_reverts(
            &cfg,
            CheckKind::Test,
            Some("linux")
        )
    );
    assert!(
        !rosen_server::post_land::failures::confirmed_failure_reverts(
            &cfg,
            CheckKind::Build,
            Some("linux")
        )
    );
}
