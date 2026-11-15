//! R7/R8/R12 risk: gating after-land changes or reverting test failures stalls
//! development; bypassing head/conflict/CAS checks or reverting other lands loses work.
use rosen_core::{AgentId, Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_protocol::*;
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::{sync::Arc, time::Instant};

fn setup(path: &std::path::Path) -> Arc<ServerState> {
    let state = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("after-land-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let config = b"review = \"after-land\"\n[checks.pre_land]\ncommands = [\"build\"]\n[checks.post_land]\ncommands = [\"test\"]\n";
    let blob = store.put_blob(config).unwrap();
    let tree = store
        .put_tree(
            &Tree::new(vec![
                TreeEntry::new(FileMode::Regular, "rosen.toml", blob).unwrap()
            ])
            .unwrap(),
        )
        .unwrap();
    let who = Signature::new("test", "test@test", 0);
    let root = store
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
        .init_base("main", &root)
        .unwrap();
    state
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        other => panic!("{other:?}"),
    }
}

async fn land(state: &Arc<ServerState>, path: &str) -> (String, ObjectId, ObjectId) {
    land_bytes(state, path, b"work\n").await
}

async fn land_bytes(
    state: &Arc<ServerState>,
    path: &str,
    data: &[u8],
) -> (String, ObjectId, ObjectId) {
    let Response::Change(c) = call(
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
    let id = c.change.id.to_string();
    call(
        state,
        Request::WorkspaceWrite(WorkspaceWrite {
            change: id.clone(),
            path: path.into(),
            data: Some(WorkspaceBytes(data.to_vec())),
        }),
    )
    .await;
    let Response::Change(c) = call(state, Request::ChangeShow { change: id.clone() }).await else {
        panic!()
    };
    let started = Instant::now();
    let Response::Land(answer) = call(state, Request::Land { change: id.clone() }).await else {
        panic!()
    };
    eprintln!(
        "after-land without receipt or approval: {:?}",
        started.elapsed()
    );
    let LandStatus::Landed { commit, .. } = answer.outcome else {
        panic!("{answer:?}")
    };
    state.landing.settle();
    (id, c.change.head, commit)
}

async fn fail(state: &Arc<ServerState>, kind: CheckKind) -> BisectState {
    let request = if kind == CheckKind::Build {
        Request::PostLandBuildNext
    } else {
        Request::PostLandNext
    };
    let Response::PostLand(work) = call(state, request).await else {
        panic!()
    };
    let job = work.job.unwrap();
    assert_eq!(job.kind, kind);
    assert_eq!(
        job.check_set.commands,
        vec![if kind == CheckKind::Build {
            "build"
        } else {
            "test"
        }]
    );
    let Response::Bisection(answer) = call(
        state,
        Request::PostLandFailed {
            job,
            output: "first failure".into(),
        },
    )
    .await
    else {
        panic!()
    };
    answer
}

async fn confirm(
    state: &Arc<ServerState>,
    mut answer: BisectState,
    culprit: ObjectId,
) -> BisectState {
    while let Some(job) = answer.job {
        assert_eq!(job.kind, answer.kind);
        let result = BisectResult {
            passed: job.commit != culprit,
            job,
            output: "confirmed failure".into(),
        };
        let Response::Bisection(next) = call(state, Request::BisectSubmit(result)).await else {
            panic!()
        };
        answer = next;
    }
    assert_eq!(answer.culprit.as_ref().unwrap().commit, culprit);
    answer
}

async fn failures(state: &Arc<ServerState>, change: &str) -> Vec<PostLandFailure> {
    let Response::PostLandFailures(found) = call(
        state,
        Request::PostLandFailures {
            change: Some(change.into()),
        },
    )
    .await
    else {
        panic!()
    };
    found
}

#[tokio::test]
async fn after_land_records_tests_and_reverts_only_builds_across_restart() {
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    assert_separate_build_progress(&state).await;
    let (id, head, culprit) = land(&state, "bad.rs").await;
    review_landed(&state, &id, head).await;
    let test_session = fail(&state, CheckKind::Test).await;
    let build_session = fail(&state, CheckKind::Build).await;
    assert_ne!(test_session.id, build_session.id);
    // Persist a partly completed bisection and resume it through the public API.
    let state = restart(state);
    let Response::Bisection(test_session) = call(
        &state,
        Request::BisectNext {
            session: test_session.id,
        },
    )
    .await
    else {
        panic!()
    };
    let done = confirm(&state, test_session, culprit).await;
    let state = restart(state);
    let recorded = failures(&state, &id).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].kind, CheckKind::Test);
    assert_eq!(recorded[0].revert, None);
    assert!(!recorded[0].requires_revert);
    assert_eq!(recorded[0].output, "confirmed failure");
    assert_eq!(
        Ledger::open(&state.config.metadata_path())
            .unwrap()
            .base("main")
            .unwrap(),
        Some(culprit)
    );
    let Response::Change(c) = call(&state, Request::ChangeShow { change: id.clone() }).await else {
        panic!()
    };
    assert_eq!(c.change.state, rosen_core::ChangeState::Landed);
    call(&state, Request::BisectNext { session: done.id }).await;
    assert_eq!(failures(&state, &id).await.len(), 1);
    let Response::PostLand(work) = call(&state, Request::PostLandNext).await else {
        panic!()
    };
    assert!(work.job.is_none());
    let (_, _, later) = land(&state, "later.rs").await;
    confirm(&state, build_session, culprit).await;
    assert_build_inverse(&state, &id, later).await;
}

async fn assert_build_inverse(state: &Arc<ServerState>, id: &str, later: ObjectId) {
    let records = failures(state, id).await;
    assert_eq!(records.len(), 2);
    let inverse = records[1].revert.unwrap();
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    assert_eq!(store.read_commit(&inverse).unwrap().parents, vec![later]);
    let tree = store
        .read_tree(&store.read_commit(&inverse).unwrap().tree)
        .unwrap();
    assert!(tree.entries().iter().any(|e| e.name == b"later.rs"));
    assert!(!tree.entries().iter().any(|e| e.name == b"bad.rs"));
    let Response::Change(c) = call(state, Request::ChangeShow { change: id.into() }).await else {
        panic!()
    };
    assert_eq!(c.change.state, rosen_core::ChangeState::Open);
}

async fn review_landed(state: &Arc<ServerState>, id: &str, head: ObjectId) {
    call(
        state,
        Request::ReviewApprove(ApproveRequest {
            change: id.into(),
            head,
            reviewer: AgentId::new("reviewer").unwrap(),
        }),
    )
    .await;
    call(
        state,
        Request::ReviewComment(CommentRequest {
            change: id.into(),
            head: Some(head),
            author: AgentId::new("reviewer").unwrap(),
            body: "review after landing".into(),
            target: CommentTarget::Line {
                path: "bad.rs".into(),
                line: 1,
            },
        }),
    )
    .await;
    let Response::LandedReview(marked) = call(
        state,
        Request::ReviewMark(MarkReviewed {
            change: id.into(),
            state: rosen_core::ReviewedState::IssuesFiled,
            by: AgentId::new("reviewer").unwrap(),
        }),
    )
    .await
    else {
        panic!()
    };
    assert_eq!(marked.state, rosen_core::ReviewedState::IssuesFiled);
    let Response::LandedReviews(found) = call(
        state,
        Request::ReviewLanded(LandedFilter {
            state: Some(rosen_core::ReviewedState::IssuesFiled),
        }),
    )
    .await
    else {
        panic!()
    };
    assert_eq!(found, vec![marked]);
}

async fn assert_separate_build_progress(state: &Arc<ServerState>) {
    let (_, _, good) = land(state, "good.rs").await;
    let Response::PostLand(build) = call(state, Request::PostLandBuildNext).await else {
        panic!()
    };
    let Response::PostLand(passed) = call(state, Request::PostLandPassed(build.job.unwrap())).await
    else {
        panic!()
    };
    assert_eq!(passed.last_green, Some(good));
    let Response::PostLand(tests) = call(state, Request::PostLandNext).await else {
        panic!()
    };
    assert_eq!(tests.last_green, None);
    assert_eq!(tests.job.unwrap().commit, good);
}

fn restart(state: Arc<ServerState>) -> Arc<ServerState> {
    let config = state.config.clone();
    drop(state);
    ServerState::new(config)
}

// R12 must use configuration once landed, including a change that changes review mode.
#[tokio::test]
async fn landed_configuration_selects_failure_policy() {
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    let config = b"review = 'before-land'\n[checks.pre_land]\ncommands = ['build']\n[checks.post_land]\ncommands = ['test']\n";
    let (id, _, culprit) = land_bytes(&state, "rosen.toml", config).await;
    let session = fail(&state, CheckKind::Test).await;
    confirm(&state, session, culprit).await;
    let records = failures(&state, &id).await;
    assert_eq!(records.len(), 1);
    assert!(records[0].requires_revert);
    assert!(records[0].revert.is_some());
}
