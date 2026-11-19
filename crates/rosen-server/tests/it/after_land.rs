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
    let config = b"review = \"after-land\"\ngenerated = [\"artifact.lock\"]\n[checks.pre_land]\ncommands = [\"build\"]\n[checks.post_land]\ncommands = [\"test\"]\n";
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
    let (id, head, answer) = land_answer(state, path, data).await;
    let LandStatus::Landed { commit, .. } = answer.outcome else {
        panic!("{answer:?}")
    };
    state.landing.settle();
    (id, head, commit)
}

/// Writes `data` at `path` in a new change and lands it, answering the
/// change, its head and the land's answer.
async fn land_answer(
    state: &Arc<ServerState>,
    path: &str,
    data: &[u8],
) -> (String, ObjectId, LandAnswer) {
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
    (id, c.change.head, answer)
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
            submitter: None,
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

#[tokio::test(flavor = "multi_thread")]
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

// R12 risk (#373): overlapping batches must not multiply reports or lose
// completed batch coverage when legacy duplicate records are consolidated.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_test_failures_are_recorded_once() {
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    let (id, _, culprit) = land(&state, "bad.rs").await;
    let first = fail(&state, CheckKind::Test).await;
    land(&state, "later.rs").await;
    let second = fail(&state, CheckKind::Test).await;
    assert_ne!(first.id, second.id);
    confirm(&state, first, culprit).await;
    let done = confirm(&state, second, culprit).await;
    assert_eq!(failures(&state, &id).await.len(), 1);
    // Recreate pre-upgrade rows: a later session reported this same culprit.
    let conn = rusqlite::Connection::open(state.config.metadata_path()).unwrap();
    conn.execute_batch("DROP INDEX post_land_failure_culprit")
        .unwrap();
    conn.execute(
        "INSERT INTO post_land_failures(session,change_id,commit_id,kind,output,
         requires_revert,revert,submitter,submitted_at)
         SELECT ?1,change_id,commit_id,kind,output,requires_revert,revert,
         submitter,submitted_at FROM post_land_failures",
        [done.id],
    )
    .unwrap();
    drop(conn);
    let state = restart(state);
    assert_eq!(failures(&state, &id).await.len(), 1);
    let Response::PostLand(work) = call(&state, Request::PostLandNext).await else {
        panic!()
    };
    assert!(work.job.is_none() && work.last_green.is_none());
    let events = state.events.page(0, None, 100).unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|e| e.kind == EventKind::PostLandFailure)
            .count(),
        1
    );
}

// R12 risk (#373): the old test green hides a later regression behind a
// recorded culprit. A known failure is a search bound, never a green prefix.
#[tokio::test(flavor = "multi_thread")]
async fn later_test_search_starts_after_recorded_culprit() {
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    let (first_id, _, first) = land(&state, "bad.rs").await;
    let session = fail(&state, CheckKind::Test).await;
    confirm(&state, session, first).await;
    let state = restart(state);
    let (_, _, repair) = land_bytes(&state, "bad.rs", b"fixed\n").await;
    let (second_id, _, second) = land(&state, "second.rs").await;
    let session = fail(&state, CheckKind::Test).await;
    assert_eq!(session.job.as_ref().unwrap().commit, repair);
    confirm(&state, session, second).await;
    assert_eq!(failures(&state, &first_id).await.len(), 1);
    assert_eq!(failures(&state, &second_id).await.len(), 1);
    let Response::PostLand(work) = call(&state, Request::PostLandNext).await else {
        panic!()
    };
    assert!(work.job.is_none() && work.last_green.is_none());
}

// R12 must use configuration once landed, including a change that changes review mode.
#[tokio::test(flavor = "multi_thread")]
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

// R12 risk (#390): one unrevertable culprit must not mask a later build break.
#[tokio::test(flavor = "multi_thread")]
async fn refused_build_inverse_does_not_mask_the_next_culprit() {
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    let (first_id, _, first) = land(&state, "bad.rs").await;
    let (_, _, overlap) = land_bytes(&state, "bad.rs", b"overlapping work\n").await;
    let session = fail(&state, CheckKind::Build).await;
    let first_done = confirm(&state, session, first).await;
    let first_record = failures(&state, &first_id).await.remove(0);
    assert!(first_record.requires_revert && first_record.revert.is_none());
    assert_refusal_details(&state, first, overlap);
    assert_pending_inverse(&state, first, overlap).await;
    assert_refusal_deduplicated(&state, first_done.id).await;
    let (second_id, _, second) = land(&state, "second.rs").await;
    let session = fail(&state, CheckKind::Build).await;
    assert_eq!(session.job.as_ref().unwrap().commit, overlap);
    let state = restart(state);
    let Response::Bisection(session) = call(
        &state,
        Request::BisectNext {
            session: session.id,
        },
    )
    .await
    else {
        panic!()
    };
    confirm_with_known_build_failure(&state, session, first, second).await;
    assert!(failures(&state, &first_id).await[0].revert.is_none());
    assert!(failures(&state, &second_id).await[0].revert.is_some());
    assert_known_build_batch_is_covered(&state).await;
    let state = restart(state);
    let Response::PostLand(work) = call(&state, Request::PostLandBuildNext).await else {
        panic!()
    };
    assert!(work.job.is_none() && work.last_green.is_none());
}

async fn confirm_with_known_build_failure(
    state: &Arc<ServerState>,
    mut session: BisectState,
    first: ObjectId,
    second: ObjectId,
) {
    while let Some(job) = session.job {
        assert_ne!(job.commit, first);
        // The parent still fails with the recorded first culprit's output.
        let output = if job.commit == second {
            "second failure"
        } else {
            "confirmed failure"
        };
        let Response::Bisection(next) = call(
            state,
            Request::BisectSubmit(BisectResult {
                job,
                passed: false,
                output: output.into(),
                submitter: None,
            }),
        )
        .await
        else {
            panic!()
        };
        session = next;
    }
    assert_eq!(session.culprit.unwrap().commit, second);
}

async fn assert_known_build_batch_is_covered(state: &Arc<ServerState>) {
    let Response::PostLand(work) = call(state, Request::PostLandBuildNext).await else {
        panic!()
    };
    assert!(work.last_green.is_none());
    let Response::Bisection(known) = call(
        state,
        Request::PostLandFailed {
            job: work.job.unwrap(),
            output: "confirmed failure".into(),
        },
    )
    .await
    else {
        panic!()
    };
    assert!(known.job.is_none() && known.culprit.is_none() && !known.flaky);
}

fn assert_refusal_details(state: &Arc<ServerState>, first: ObjectId, overlap: ObjectId) {
    let refusal = state
        .events
        .page(0, None, 100)
        .unwrap()
        .events
        .into_iter()
        .find(|event| event.kind == EventKind::RevertRefused)
        .unwrap();
    assert_eq!(refusal.detail["culprit"], first.to_hex());
    assert_eq!(
        refusal.detail["overlapping_commits"],
        serde_json::json!([overlap])
    );
    assert_eq!(refusal.detail["paths"], serde_json::json!(["bad.rs"]));
    assert!(refusal.detail["reason"]
        .as_str()
        .unwrap()
        .contains("conflict"));
}

async fn assert_refusal_deduplicated(state: &Arc<ServerState>, session: i64) {
    // Replaying the same refusal at the same base must not publish it again.
    call(state, Request::BisectNext { session }).await;
    assert_eq!(
        state
            .events
            .page(0, None, 100)
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind == EventKind::RevertRefused)
            .count(),
        1
    );
}

// Risk (R5/R8, #389): a generated merge must not publish or certify the
// target base's artifact while losing the owner's regeneration requirement.
#[tokio::test(flavor = "multi_thread")]
async fn generated_merge_refuses_until_owner_rebases_and_regenerates() {
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    let Response::Change(record) = call(
        &state,
        Request::ChangeNew(NewChange {
            owner: AgentId::new("owner").unwrap(),
            on: None,
        }),
    )
    .await
    else {
        panic!()
    };
    let id = record.change.id.to_string();
    write_artifact(&state, &id, b"owner artifact").await;
    let (_, _, base) = land_bytes(&state, "artifact.lock", b"base artifact").await;
    let Response::MergeResult(preview) =
        call(&state, Request::MergeResult { change: id.clone() }).await
    else {
        panic!()
    };
    assert_eq!(preview.regenerate, vec!["artifact.lock"]);
    assert!(preview.tree.is_some());
    let receipt = rosen_core::Receipt {
        change: preview.change,
        head: preview.head,
        base: preview.base,
        check_set: "pre-land".into(),
        submitter: AgentId::new("checker").unwrap(),
    };
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    assert!(ledger.checked_merge_result(&receipt).unwrap().is_none());
    let Response::Land(answer) = call(&state, Request::Land { change: id.clone() }).await else {
        panic!()
    };
    let LandStatus::Refused {
        reason,
        paths,
        message,
    } = answer.outcome
    else {
        panic!()
    };
    assert_eq!(reason, "regeneration_required");
    assert_eq!(paths, vec!["artifact.lock"]);
    assert!(message.contains(&id) && message.contains("rosen rebase"));
    assert_eq!(ledger.base("main").unwrap(), Some(base));
    assert!(state.changes.show(&id).unwrap().change.conflicts.is_empty());
    assert_generated_repair(&state, &id).await;
}

async fn assert_generated_repair(state: &Arc<ServerState>, id: &str) {
    call(state, Request::ChangeRebase { change: id.into() }).await;
    write_artifact(state, id, b"regenerated from merged inputs").await;
    let Response::Land(answer) = call(state, Request::Land { change: id.into() }).await else {
        panic!()
    };
    let LandStatus::Landed { commit, .. } = answer.outcome else {
        panic!()
    };
    state.landing.settle();
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let tree = store
        .read_tree(&store.read_commit(&commit).unwrap().tree)
        .unwrap();
    let artifact = tree
        .entries()
        .iter()
        .find(|e| e.name == b"artifact.lock")
        .unwrap();
    assert_eq!(
        store.read_blob(&artifact.id).unwrap().data,
        b"regenerated from merged inputs"
    );
}

async fn write_artifact(state: &Arc<ServerState>, id: &str, data: &[u8]) {
    call(
        state,
        Request::WorkspaceWrite(WorkspaceWrite {
            change: id.into(),
            path: "artifact.lock".into(),
            data: Some(WorkspaceBytes(data.into())),
        }),
    )
    .await;
}

/// Risk (#397): a land with no merge kept a Windows editor's CRLF rewrite of
/// an LF file, so the mirror showed every line changed for a one-line edit.
#[tokio::test(flavor = "multi_thread")]
async fn land_with_no_merge_keeps_the_base_line_endings() {
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    land_bytes(&state, "notes.md", b"one\ntwo\n").await;
    // The change sits directly on the base, so the land runs no merge.
    let (_, _, commit) = land_bytes(&state, "notes.md", b"one\r\ntwo\r\nthree\r\n").await;
    assert_eq!(
        landed_file(&state, commit, "notes.md"),
        b"one\ntwo\nthree\n"
    );
}

/// Risk (#424): keeping the base's line endings left files stored CRLF by an
/// older server stuck, since a land converting them to LF stored no change.
/// A declared `eol=lf` must win both ways, the land must say so, and a
/// `-text` file must keep its bytes.
#[tokio::test(flavor = "multi_thread")]
async fn land_follows_declared_gitattributes_line_endings() {
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    // Stored CRLF before the project declared its line endings.
    land_bytes(&state, "stuck.md", b"one\r\ntwo\r\n").await;
    let attributes = b"* text=auto eol=lf\n*.dat -text\n";
    land_bytes(&state, ".gitattributes", attributes).await;
    let (_, _, repaired) = land_bytes(&state, "stuck.md", b"one\ntwo\n").await;
    assert_eq!(landed_file(&state, repaired, "stuck.md"), b"one\ntwo\n");
    let crlf = b"one\r\ntwo\r\nthree\r\n";
    let (_, _, answer) = land_answer(&state, "stuck.md", crlf).await;
    let LandStatus::Landed {
        commit,
        line_endings,
        ..
    } = answer.outcome
    else {
        panic!("{answer:?}")
    };
    state.landing.settle();
    assert_eq!(
        landed_file(&state, commit, "stuck.md"),
        b"one\ntwo\nthree\n"
    );
    let rewrite = LineEndingRewrite {
        path: "stuck.md".into(),
        eol: "lf".into(),
    };
    assert_eq!(line_endings, vec![rewrite]);
    let (_, _, kept) = land_bytes(&state, "raw.dat", b"a\r\nb\r\n").await;
    assert_eq!(landed_file(&state, kept, "raw.dat"), b"a\r\nb\r\n");
}

/// The bytes of the root file `name` in `commit`'s tree.
fn landed_file(state: &Arc<ServerState>, commit: ObjectId, name: &str) -> Vec<u8> {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let tree = store
        .read_tree(&store.read_commit(&commit).unwrap().tree)
        .unwrap();
    let entry = tree
        .entries()
        .iter()
        .find(|e| e.name == name.as_bytes())
        .unwrap();
    store.read_blob(&entry.id).unwrap().data
}

async fn status(state: &Arc<ServerState>) -> PostLandStatus {
    let Response::PostLandStatus(status) = call(state, Request::PostLandStatus).await else {
        panic!()
    };
    status
}

/// Risk (#407): a refused inverse that vanishes from the status hides a
/// broken base from whoever must repair it.
async fn assert_pending_inverse(state: &Arc<ServerState>, first: ObjectId, overlap: ObjectId) {
    let status = status(state).await;
    let pending = &status.pending_inverses[0];
    assert_eq!(pending.culprit, first);
    assert_eq!(pending.paths, vec!["bad.rs".to_owned()]);
    assert_eq!(pending.overlapping_commits, vec![overlap]);
    assert!(pending.attempted_base.is_some() && pending.first_refused_at.is_some());
}

/// Risk (#407): a status read that moves a bisection, a lost progress event,
/// or a submitter's machine taken from the request misleads whoever watches
/// post-land work (R12, R9, R14 "Agents").
#[tokio::test(flavor = "multi_thread")]
async fn bisection_status_events_and_submitters() {
    use rosen_server::{receipts::Origin, rpc::dispatch_from};
    let temp = tempfile::tempdir().unwrap();
    let state = setup(temp.path());
    let mut landed = Vec::new();
    for path in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        landed.push(land(&state, path).await.2);
    }
    let (machine, credential) = enroll(temp.path());
    let remote = |request| dispatch_from(&state, request, Origin::Machine(credential.clone()));
    let Response::PostLand(work) = call(&state, Request::PostLandNext).await else {
        panic!()
    };
    let mut job = work.job.unwrap();
    job.submitter = Some(AgentId::new("test-3").unwrap());
    let request = Request::PostLandFailed {
        job,
        output: "first failure".into(),
    };
    let Outcome::Ok(Response::Bisection(mut answer)) = remote(request).await else {
        panic!()
    };
    let before = status(&state).await;
    assert_eq!(
        before,
        status(&state).await,
        "a status read changes nothing"
    );
    let session = &before.sessions[0];
    assert_eq!((session.interval, session.planned_steps), (4, 4));
    assert_eq!(session.opened_by, Some(format!("test-3@{machine}")));
    let states: Vec<_> = session.steps.iter().map(|step| step.state).collect();
    assert_eq!(
        states,
        [
            StepState::Offered,
            StepState::Waiting,
            StepState::Waiting,
            StepState::Waiting
        ]
    );
    let mut accepted = 0;
    while let Some(job) = answer.job.clone() {
        let result = BisectResult {
            passed: !landed[2..].contains(&job.commit),
            job,
            output: "confirmed failure".into(),
            // The request cannot name another machine.
            submitter: Some(AgentId::new("test-3@elsewhere").unwrap()),
        };
        let refused = remote(Request::BisectSubmit(result.clone())).await;
        assert!(matches!(refused, Outcome::Error(e) if e.code == ErrorCode::InvalidArgument));
        let result = BisectResult {
            submitter: Some(AgentId::new("test-3").unwrap()),
            ..result
        };
        let Outcome::Ok(Response::Bisection(next)) = remote(Request::BisectSubmit(result)).await
        else {
            panic!()
        };
        answer = next;
        accepted += 1;
        if accepted == 1 {
            let step = &status(&state).await.sessions[0].steps[0];
            assert_eq!(step.submitter, Some(format!("test-3@{machine}")));
        }
    }
    assert_eq!(answer.culprit.unwrap().commit, landed[2]);
    assert!(status(&state).await.sessions.is_empty());
    let progress: Vec<String> = state
        .events
        .page(0, None, 100)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == EventKind::Bisect)
        .map(|event| event.detail["progress"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(progress, ["opened", "step", "step", "step", "culprit"]);
    let owner = AgentId::new("owner").unwrap();
    let owned = state.events.page(0, Some(&owner), 100).unwrap().events;
    assert!(owned.iter().all(|event| event.kind != EventKind::Bisect));
}

/// Enrolls a checker machine and returns its ID and credential secret.
fn enroll(dir: &std::path::Path) -> (String, String) {
    use rosen_server::machines::{credentials, keys};
    let db = rosen_server::machines::open(dir).unwrap();
    db.execute(
        "INSERT INTO join_keys(id,secret_hash,expires_at,remaining,role)
         VALUES ('k',?1,9999999999,1,'checker')",
        [keys::digest("join-secret")],
    )
    .unwrap();
    let credential = credentials::enroll(dir, "join-secret", "forge-06", None).unwrap();
    (credential.machine_id, credential.secret)
}
