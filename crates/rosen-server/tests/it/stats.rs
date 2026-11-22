//! Persistence and query coverage for event timestamps (`at_ms`), run/check/land
//! timings, `Request::Stats`, and Collapse state-feed `stats` aggregates (#449).
//!
//! Risk: timing data held only in memory vanishes across server restarts,
//! leaving Velocity and agents blind to land latency, queue wait and failure
//! rates; missing `at_ms` forces clients to read SQLite directly.

use std::path::Path;
use std::sync::Arc;

use rosen_core::{AgentId, ChangeId, Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_protocol::*;
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};

fn init_server(data_dir: &Path) -> (Arc<ServerState>, ObjectId) {
    let state = ServerState::new(ServerConfig {
        data_dir: data_dir.into(),
        endpoint: Endpoint::new("stats-test"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let cfg = "review = \"after-land\"\n[checks.post_land]\ncommands = [\"cargo test\"]\n";
    let entries = vec![
        TreeEntry::new(
            FileMode::Regular,
            "rosen.toml",
            store.put_blob(cfg.as_bytes()).unwrap(),
        )
        .unwrap(),
        TreeEntry::new(
            FileMode::Regular,
            "a.txt",
            store.put_blob(b"base\n").unwrap(),
        )
        .unwrap(),
    ];
    let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
    let who = Signature::new("test", "test@example.com", 1_700_000_000);
    let commit = store
        .put_commit(&Commit {
            tree,
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"initial\n".to_vec(),
        })
        .unwrap();
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .init_base("main", &commit)
        .unwrap();
    (state, commit)
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        other => panic!("unexpected outcome: {other:?}"),
    }
}

async fn new_change(state: &Arc<ServerState>, owner: &str) -> ChangeId {
    let Response::Change(record) = call(
        state,
        Request::ChangeNew(NewChange {
            owner: AgentId::new(owner).unwrap(),
            on: None,
        }),
    )
    .await
    else {
        panic!("expected Change")
    };
    record.change.id
}

async fn write_and_name(
    state: &Arc<ServerState>,
    id: ChangeId,
    path: &str,
    data: &[u8],
    message: &str,
) {
    let Response::Snapshot(written) = call(
        state,
        Request::WorkspaceWrite(WorkspaceWrite {
            change: id.to_string(),
            path: path.into(),
            data: Some(WorkspaceBytes(data.to_vec())),
        }),
    )
    .await
    else {
        panic!("expected Snapshot")
    };
    let head = written.change.change.head;
    call(
        state,
        Request::Snapshot(SnapshotRequest {
            change: id.to_string(),
            tree: head,
            from: Some(head),
            message: Some(message.into()),
        }),
    )
    .await;
}

async fn land(state: &Arc<ServerState>, id: ChangeId) -> ObjectId {
    let Response::Land(answer) = call(
        state,
        Request::Land {
            change: id.to_string(),
        },
    )
    .await
    else {
        panic!("expected Land")
    };
    state.landing.settle();
    match answer.outcome {
        LandStatus::Landed { commit, .. } => commit,
        other => panic!("expected landed, got {other:?}"),
    }
}

/// Risk (#449): older clients/servers omit `at_ms`, while current servers must
/// expose Unix ms from `events.created_at` on every delivered `Event`.
#[test]
fn event_at_ms_defaults_for_older_peers_and_exposes_created_at() {
    let legacy = r#"{"seq":1,"kind":"mirror","change":null,"detail":{}}"#;
    let decoded: Event = serde_json::from_str(legacy).unwrap();
    assert_eq!(decoded.at_ms, 0);

    let dir = tempfile::tempdir().unwrap();
    let log = rosen_server::events::EventLog::open(dir.path().join("metadata.sqlite"));
    let seq = log.publish(EventKind::Mirror, None, serde_json::json!({"ok": true}));
    assert_eq!(seq, 1);
    let events = log.after(0);
    assert_eq!(events.len(), 1);
    assert!(events[0].at_ms > 1_600_000_000_000, "{:?}", events[0]);
}

/// Risk (#449): per-change timelines, land phase totals, run-queue wait/run
/// stats, post-land checkpoints and window aggregates must all survive a server
/// restart and appear in the Collapse state feed `stats` document.
#[tokio::test(flavor = "multi_thread")]
async fn stats_and_timings_survive_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _base) = init_server(dir.path());
    let now = rosen_server::stats::now_millis();

    let id_a = new_change(&state, "alice").await;
    write_and_name(&state, id_a, "a.txt", b"alice\n", "Change A").await;
    let commit_a = land(&state, id_a).await;

    let id_b = new_change(&state, "bob").await;
    write_and_name(&state, id_b, "b.txt", b"bob\n", "Change B").await;
    let commit_b = land(&state, id_b).await;

    // Record run-queue timings for change A (one fresh, one reused).
    for (wait, run, reused, exit) in [(50, 200, false, Some(0)), (10, 5, true, Some(0))] {
        call(
            &state,
            Request::RecordRunTiming(RecordRunTiming {
                label: "cargo clippy".into(),
                class: "build".into(),
                change: Some(id_a.to_string()),
                queued_at_ms: now - 5_000,
                started_at_ms: now - 5_000 + wait,
                finished_at_ms: now - 5_000 + wait + run,
                exit_status: exit,
                reused,
            }),
        )
        .await;
    }

    // Advance post-land test green covering both lands and record durations.
    let Response::PostLand(work) = call(&state, Request::PostLandNext).await else {
        panic!("expected PostLand")
    };
    let job = work.job.expect("pending post-land job");
    assert_eq!(job.commit, commit_b);
    call(&state, Request::PostLandPassed(job)).await;
    call(
        &state,
        Request::RecordPostLandTiming(RecordPostLandTiming {
            commit: commit_b,
            kind: CheckKind::Test,
            platform: None,
            bisect_session: None,
            build_ms: Some(120),
            lint_ms: Some(45),
            test_ms: Some(330),
        }),
    )
    .await;

    // Events must carry real `at_ms`.
    let events = state.events.after(0);
    assert!(!events.is_empty());
    assert!(events.iter().all(|e| e.at_ms > 1_600_000_000_000));

    // Simulate a server restart on the same data directory.
    drop(state);
    let restarted = ServerState::new(ServerConfig {
        data_dir: dir.path().into(),
        endpoint: Endpoint::new("stats-test-restarted"),
        warm_pool_dir: None,
    });

    let Response::Stats(report) = call(
        &restarted,
        Request::Stats(StatsQuery {
            since_ms: Some(now - 3_600_000),
            change: None,
        }),
    )
    .await
    else {
        panic!("expected Stats")
    };

    assert_eq!(report.window.lands, 2);
    assert!(report.window.land_latency_p50_ms.is_some());
    assert!(report.window.land_latency_p95_ms.is_some());
    assert_eq!(report.window.queue_wait_by_label.len(), 1);
    let clippy = &report.window.queue_wait_by_label[0];
    assert_eq!(clippy.label, "cargo clippy");
    assert_eq!(clippy.runs, 2);
    assert_eq!(clippy.reused, 1);
    assert_eq!(clippy.wait_p50_ms, Some(10));
    assert_eq!(clippy.wait_p95_ms, Some(50));

    assert_eq!(report.changes.len(), 2);
    let stats_a = report
        .changes
        .iter()
        .find(|c| c.change == id_a.to_string())
        .expect("change A stats");
    assert!(stats_a.timeline.snapshot_at_ms.is_some());
    assert!(stats_a.timeline.named_at_ms.is_some());
    assert!(stats_a.timeline.landed_at_ms.is_some());
    assert!(stats_a.timeline.green_at_ms.is_some());
    assert_eq!(stats_a.run_count, 2);
    assert_eq!(stats_a.run_wait_ms, 60);
    assert_eq!(stats_a.run_duration_ms, 205);
    assert!(stats_a.land_phases.is_some());

    assert_eq!(report.checkpoints.len(), 1);
    let cp = &report.checkpoints[0];
    assert_eq!(cp.commit, commit_b);
    assert_eq!(cp.batch_size, 2);
    assert_eq!(cp.outcome, "passed");
    assert_eq!(cp.build_ms, Some(120));
    assert_eq!(cp.lint_ms, Some(45));
    assert_eq!(cp.test_ms, Some(330));

    // Per-change filter resolves a short prefix too.
    let Response::Stats(filtered) = call(
        &restarted,
        Request::Stats(StatsQuery {
            since_ms: None,
            change: Some(id_a.to_string()[..8].to_owned()),
        }),
    )
    .await
    else {
        panic!("expected filtered Stats")
    };
    assert_eq!(filtered.changes.len(), 1);
    assert_eq!(filtered.changes[0].change, id_a.to_string());
    assert_ne!(commit_a, commit_b);

    // State feed includes `stats` aggregates for Collapse (R9).
    let conn = rosen_server::metadata::open(&restarted.config.metadata_path()).unwrap();
    let window =
        rosen_server::stats::query::window_aggregates(&conn, Some(now - 3_600_000), now + 1_000)
            .unwrap();
    assert_eq!(window.lands, 2);
    assert_eq!(window.post_land_checks, 1);
}
