//! Golden wire fixtures for the Collapse contract (`fixtures/collapse/v1/`,
//! issue #436, SPEC R1, R9, R13, R14).
//!
//! Risk: Rosen's Collapse senders and Collapse's intake live in two
//! repositories. If a sender changes an envelope key, document field, route
//! path or payload type without updating the shared fixtures, Collapse
//! silently refuses or misreads live batches.
//!
//! Each fixture is checked against the real sender code as parsed JSON. On a
//! mismatch the test prints the complete actual JSON so a fix-forward thread
//! can update the fixture directly, or run with `UPDATE_FIXTURES=1` to rewrite
//! the files:
//!
//! `UPDATE_FIXTURES=1 cargo test -p rosen-server --lib collapse_fixtures`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rosen_core::conflict::{Conflict, ConflictSet};
use rosen_core::{
    AgentId, ChangeId, Commit, FileMode, ObjectId, ReviewedState, Signature, Tree, TreeEntry,
};
use rosen_protocol::channel::{
    AppendEntry, Author, ChannelAnswer, ChannelRequest, Intent, Ledger as ChannelLedger,
    MessageMode, PieceOutcome, SendMessage,
};
use rosen_protocol::*;
use rosen_store::{LooseObjectStore, ObjectStore};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::landing::{self, LandError, Ledger};
use crate::ledger_push::{self, Session as LedgerSession};
use crate::post_land::{self, submitter::Submitted};
use crate::relay::{
    action,
    remote::{Fetched, Remote},
    store::RelayStore,
};
use crate::rpc::dispatch;
use crate::state_push::{
    batch, change_doc,
    facts::Stores,
    log_lines, machines_doc, pass, review_docs,
    unit::{Section, Unit},
};
use crate::{bisect, channel, metadata, mirror, reviews, ServerConfig, ServerState};

/// Wire schema version matching `state_push::batch::FORMAT = 1`.
const SCHEMA_VERSION: &str = "v1";
const COLLAPSE_ROOT: &str = "https://api.collapse.sh/example/project";
const FIXED_SECS: i64 = 1_700_000_000;
const FIXED_MS: i64 = 1_700_000_000_000;
const CHANGE_A: &str = "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk";
const CHANGE_B: &str = "llllllllllllllllllllllllllllllll";
const CHANGE_C: &str = "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
const COMMIT_1: &str = "1111111111111111111111111111111111111111";
const COMMIT_2: &str = "2222222222222222222222222222222222222222";
const COMMIT_3: &str = "3333333333333333333333333333333333333333";

/// Exact-value stabilizer for runtime-minted IDs, commits and timestamps.
///
/// Matches only exact runtime values returned by the server during setup —
/// never JSON field names — so renaming a field or changing a field's type
/// (e.g. integer ms vs RFC3339 string) still breaks the golden check.
#[derive(Default)]
struct Stabilizer {
    strings: BTreeMap<String, String>,
    ints: BTreeMap<i64, i64>,
}

impl Stabilizer {
    fn map_str(&mut self, runtime: impl Into<String>, stable: &str) {
        self.strings.insert(runtime.into(), stable.to_owned());
    }

    fn map_int(&mut self, runtime: i64, stable: i64) {
        self.ints.insert(runtime, stable);
    }

    fn apply(&self, value: &mut Value) {
        match value {
            Value::String(text) => {
                if let Some(stable) = self.strings.get(text) {
                    *text = stable.clone();
                } else {
                    for (from, to) in &self.strings {
                        if text.contains(from) {
                            *text = text.replace(from, to);
                        }
                    }
                }
            }
            Value::Number(num) => {
                if let Some(raw) = num.as_i64() {
                    if let Some(&stable) = self.ints.get(&raw) {
                        *value = Value::from(stable);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.apply(item);
                }
            }
            Value::Object(map) => {
                for item in map.values_mut() {
                    self.apply(item);
                }
            }
            Value::Null | Value::Bool(_) => {}
        }
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/collapse")
        .join(SCHEMA_VERSION)
}

fn update_mode() -> bool {
    std::env::var("UPDATE_FIXTURES")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Compares `actual` against the golden fixture as parsed JSON. On mismatch,
/// returns an error message containing the complete actual JSON.
fn check_fixture(name: &str, actual: &Value) -> Option<String> {
    let dir = fixtures_dir();
    let path = dir.join(name);
    let mut rendered = serde_json::to_string_pretty(actual).expect("pretty json");
    rendered.push('\n');
    if update_mode() {
        std::fs::create_dir_all(&dir).expect("create fixtures dir");
        std::fs::write(&path, rendered.as_bytes()).expect("write fixture");
        return None;
    }
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            return Some(format!(
                "missing golden fixture {} ({err})\n--- ACTUAL JSON ({name}) ---\n{rendered}\
                 --- END ACTUAL JSON ({name}) ---",
                path.display()
            ));
        }
    };
    let expected: Value = match serde_json::from_str(&existing) {
        Ok(val) => val,
        Err(err) => {
            return Some(format!(
                "invalid JSON in golden fixture {} ({err})\n--- ACTUAL JSON ({name}) ---\n\
                 {rendered}--- END ACTUAL JSON ({name}) ---",
                path.display()
            ));
        }
    };
    if &expected != actual {
        return Some(format!(
            "wire fixture {} drifted from sender output; run `UPDATE_FIXTURES=1 cargo test -p \
             rosen-server --lib collapse_fixtures` or paste the actual JSON below:\n\
             --- ACTUAL JSON ({name}) ---\n{rendered}--- END ACTUAL JSON ({name}) ---",
            path.display()
        ));
    }
    None
}

fn init_server(path: &Path, config: &str, files: &[(&str, &[u8])]) -> (Arc<ServerState>, ObjectId) {
    let state = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("collapse-wire-fixtures"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let mut entries = vec![TreeEntry::new(
        FileMode::Regular,
        "rosen.toml",
        store.put_blob(config.as_bytes()).unwrap(),
    )
    .unwrap()];
    for (name, bytes) in files {
        let blob = store.put_blob(bytes).unwrap();
        entries.push(TreeEntry::new(FileMode::Regular, *name, blob).unwrap());
    }
    let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
    let who = Signature::new("fixture", "fixture@example.com", FIXED_SECS);
    let commit = store
        .put_commit(&Commit {
            tree,
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"initial base\n".to_vec(),
        })
        .unwrap();
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .init_base("main", &commit)
        .unwrap();
    mirror::owner::mark(path).unwrap();
    (state, commit)
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        other => panic!("unexpected outcome: {other:?}"),
    }
}

async fn new_change(state: &Arc<ServerState>, owner: &str) -> ChangeId {
    let req = Request::ChangeNew(NewChange {
        owner: AgentId::new(owner).unwrap(),
        on: None,
    });
    let Response::Change(record) = call(state, req).await else {
        panic!()
    };
    record.change.id
}

async fn write_and_name(
    state: &Arc<ServerState>,
    id: ChangeId,
    path: &str,
    data: &[u8],
    message: &str,
) -> (ObjectId, ObjectId) {
    let write = WorkspaceWrite {
        change: id.to_string(),
        path: path.into(),
        data: Some(WorkspaceBytes(data.to_vec())),
    };
    let Response::Snapshot(written) = call(state, Request::WorkspaceWrite(write)).await else {
        panic!()
    };
    let head = written.change.change.head;
    let name = SnapshotRequest {
        change: id.to_string(),
        tree: head,
        from: Some(head),
        message: Some(message.into()),
    };
    let Response::Snapshot(named) = call(state, Request::Snapshot(name)).await else {
        panic!()
    };
    (head, named.commit.expect("named commit"))
}

fn record_commit_time(state: &ServerState, commit: ObjectId, stab: &mut Stabilizer) {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let parsed = store.read_commit(&commit).unwrap();
    let millis = parsed.committer.seconds.saturating_mul(1000);
    stab.map_int(millis, FIXED_MS);
}

fn single_batch(units: Vec<Unit>, seq: u64, delivered: u64, stab: &Stabilizer) -> Value {
    let batches = batch::split(units, seq, delivered);
    assert_eq!(batches.len(), 1, "expected one state-feed batch");
    let mut parsed: Value = serde_json::from_slice(&batches[0].body).unwrap();
    stab.apply(&mut parsed);
    parsed
}

fn stabilize_change_times(state: &ServerState, base_ms: i64) {
    let conn = metadata::open(&state.config.metadata_path()).unwrap();
    conn.execute(
        "UPDATE changes SET created_at = ?1, updated_at = ?2",
        [base_ms, base_ms + 5_000],
    )
    .unwrap();
    let _ = conn.execute("UPDATE path_claims SET created_at = ?1", [base_ms + 2_000]);
    let _ = conn.execute(
        "UPDATE events SET created_at = ?1 WHERE kind = 'conflict'",
        [base_ms + 4_000],
    );
}

async fn generate_state_feed_change() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!(
        "review = \"before-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n\
         [checks.pre_land]\ncommands = [\"cargo test\"]\n"
    );
    let (state, base) = init_server(dir.path(), &cfg, &[("a.rs", b"fn one() {}\nfn two() {}\n")]);
    let mut stab = Stabilizer::default();

    let id_a = new_change(&state, "alice").await;
    let (head_a, snap_a) = write_and_name(
        &state,
        id_a,
        "a.rs",
        b"fn one() {}\nfn ours() {}\n",
        "Add feature",
    )
    .await;
    let id_b = new_change(&state, "bob").await;
    let _ = write_and_name(
        &state,
        id_b,
        "a.rs",
        b"fn one() {}\nfn theirs() {}\n",
        "Other edit",
    )
    .await;
    claim_change(&state, id_b, &["a.rs"], false).await;
    claim_change(&state, id_a, &["a.rs"], false).await;
    claim_change(&state, id_a, &["b.rs"], true).await;
    submit_receipt(&state, id_a, head_a, base, "checker").await;
    store_conflict(&state, id_a, base);

    let id_c = new_change(&state, "carol").await;
    let (head_c, snap_c) = write_and_name(&state, id_c, "c.txt", b"done\n", "Ship c.txt").await;
    approve_change(&state, id_c, head_c, "reviewer").await;
    let landed_c = land_change(&state, id_c).await;
    state.landing.settle();

    stab.map_str(id_a.to_string(), CHANGE_A);
    stab.map_str(id_b.to_string(), CHANGE_B);
    stab.map_str(id_c.to_string(), CHANGE_C);
    stab.map_str(snap_a.to_hex(), COMMIT_1);
    stab.map_str(snap_c.to_hex(), COMMIT_2);
    stab.map_str(landed_c.to_hex(), COMMIT_3);
    record_commit_time(&state, snap_a, &mut stab);
    record_commit_time(&state, snap_c, &mut stab);
    stabilize_change_times(&state, FIXED_MS);

    let stores = Stores::new(&state.config);
    let unit_a = change_doc::build(&state, &stores, id_a).await.unwrap();
    let unit_c = change_doc::build(&state, &stores, id_c).await.unwrap();
    single_batch(vec![unit_a, unit_c], 10, 0, &stab)
}

async fn claim_change(state: &Arc<ServerState>, id: ChangeId, paths: &[&str], exclusive: bool) {
    let req = Request::ClaimPaths(ClaimRequest {
        change: id.to_string(),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        exclusive,
    });
    call(state, req).await;
}

async fn submit_receipt(
    state: &Arc<ServerState>,
    id: ChangeId,
    head: ObjectId,
    base: ObjectId,
    submitter: &str,
) {
    let req = Request::ReceiptSubmit(SubmitReceipt {
        change: id.to_string(),
        head,
        base,
        check_set: rosen_core::PRE_LAND_CHECK_SET.into(),
        submitter: AgentId::new(submitter).unwrap(),
        signature: None,
    });
    call(state, req).await;
}

async fn approve_change(state: &Arc<ServerState>, id: ChangeId, head: ObjectId, reviewer: &str) {
    let req = Request::ReviewApprove(ApproveRequest {
        change: id.to_string(),
        reviewer: AgentId::new(reviewer).unwrap(),
        head,
    });
    call(state, req).await;
}

async fn land_change(state: &Arc<ServerState>, id: ChangeId) -> ObjectId {
    let Response::Land(answer) = call(
        state,
        Request::Land {
            change: id.to_string(),
        },
    )
    .await
    else {
        panic!()
    };
    match answer.outcome {
        LandStatus::Landed { commit, .. } => commit,
        other => panic!("expected landed, got {other:?}"),
    }
}

fn store_conflict(state: &ServerState, id: ChangeId, onto: ObjectId) {
    let mut conn = metadata::open(&state.config.metadata_path()).unwrap();
    crate::conflicts::ensure_schema(&conn).unwrap();
    let mut set = ConflictSet::new();
    let conflict = Conflict::new(
        "a.rs",
        2,
        b"fn two() {}\n",
        b"fn ours() {}\n",
        b"fn theirs() {}\n",
    );
    set.record_merge("a.rs", [conflict]);
    let tx = conn.transaction().unwrap();
    crate::conflicts::save(&tx, &id.to_string(), &set).unwrap();
    tx.commit().unwrap();
    state.events.publish(
        EventKind::Conflict,
        Some(id.to_string()),
        landing::conflict_routes::event_detail(&["a.rs".to_owned()], onto),
    );
}

async fn generate_state_feed_review() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!("review = \"before-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n");
    let (state, _) = init_server(dir.path(), &cfg, &[("a.txt", b"one\ntwo\nthree\n")]);
    let mut stab = Stabilizer::default();

    let id = new_change(&state, "alice").await;
    let (head_1, snap_1) =
        write_and_name(&state, id, "a.txt", b"one\ntwo\nfour\n", "Initial draft").await;
    approve_change(&state, id, head_1, "reviewer").await;

    let (head_2, snap_2) = write_and_name(
        &state,
        id,
        "a.txt",
        b"one\ntwo updated\nfour\n",
        "Address review",
    )
    .await;
    let mut conn = metadata::open(&state.config.metadata_path()).unwrap();
    reviews::approvals::approve_on(
        &mut conn,
        &ApproveRequest {
            change: id.to_string(),
            reviewer: AgentId::new("maintainer").unwrap(),
            head: head_2,
        },
        COLLAPSE_MACHINE,
        rosen_core::ReviewMode::BeforeLand,
    )
    .unwrap();
    drop(conn);

    open_and_reply_threads(&state, id).await;
    stab.map_str(id.to_string(), CHANGE_A);
    stab.map_str(snap_1.to_hex(), COMMIT_1);
    stab.map_str(snap_2.to_hex(), COMMIT_2);
    record_commit_time(&state, snap_1, &mut stab);
    record_commit_time(&state, snap_2, &mut stab);
    stabilize_review_times(&state);

    let stores = Stores::new(&state.config);
    let unit = change_doc::build(&state, &stores, id).await.unwrap();
    single_batch(vec![unit], 20, 10, &stab)
}

async fn open_and_reply_threads(state: &Arc<ServerState>, id: ChangeId) {
    let open = CommentRequest {
        change: id.to_string(),
        author: AgentId::new("reviewer").unwrap(),
        body: "Check line two".into(),
        head: None,
        target: CommentTarget::Line {
            path: "a.txt".into(),
            line: 2,
        },
    };
    let Response::ReviewThread(thread_1) = call(state, Request::ReviewComment(open)).await else {
        panic!()
    };
    let reply = CommentRequest {
        change: id.to_string(),
        author: AgentId::new("alice").unwrap(),
        body: "Updated line two".into(),
        head: None,
        target: CommentTarget::Thread {
            thread: thread_1.id,
        },
    };
    call(state, Request::ReviewComment(reply)).await;

    let nit = CommentRequest {
        change: id.to_string(),
        author: AgentId::new("reviewer").unwrap(),
        body: "Nit on line one".into(),
        head: None,
        target: CommentTarget::Line {
            path: "a.txt".into(),
            line: 1,
        },
    };
    let Response::ReviewThread(thread_2) = call(state, Request::ReviewComment(nit)).await else {
        panic!()
    };
    let resolve = ResolveRequest {
        thread: thread_2.id,
        by: AgentId::new("alice").unwrap(),
    };
    call(state, Request::ReviewResolve(resolve)).await;
}

fn stabilize_review_times(state: &ServerState) {
    stabilize_change_times(state, FIXED_MS);
    let conn = metadata::open(&state.config.metadata_path()).unwrap();
    conn.execute(
        "UPDATE review_approvals SET created_at = ?1 + (id * 1000)",
        [FIXED_MS],
    )
    .unwrap();
    conn.execute(
        "UPDATE review_threads SET created_at = ?1 + (id * 1000), updated_at = ?1 + (id * 2000)",
        [FIXED_MS],
    )
    .unwrap();
    conn.execute(
        "UPDATE review_comments SET created_at = ?1 + (id * 1000)",
        [FIXED_MS],
    )
    .unwrap();
}

async fn generate_state_feed_coverage() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!("review = \"after-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n");
    let (state, _) = init_server(dir.path(), &cfg, &[]);
    let mut stab = Stabilizer::default();

    let id_a = new_change(&state, "alice").await;
    let _ = write_and_name(&state, id_a, "a.txt", b"one\n", "Land first").await;
    let commit_a = land_change(&state, id_a).await;
    state.landing.settle();

    let id_b = new_change(&state, "bob").await;
    let _ = write_and_name(&state, id_b, "b.rs", b"fn two() {}\n", "Land second").await;
    let commit_b = land_change(&state, id_b).await;
    state.landing.settle();

    let mark = MarkReviewed {
        change: id_b.to_string(),
        state: ReviewedState::Reviewed,
        by: AgentId::new("reviewer@collapse").unwrap(),
    };
    call(&state, Request::ReviewMark(mark)).await;

    let conn = metadata::open(&state.config.metadata_path()).unwrap();
    conn.execute(
        "UPDATE landed_reviews SET updated_at = ?1",
        [FIXED_MS + 9_000],
    )
    .unwrap();
    drop(conn);

    stab.map_str(id_a.to_string(), CHANGE_A);
    stab.map_str(id_b.to_string(), CHANGE_B);
    stab.map_str(commit_a.to_hex(), COMMIT_1);
    stab.map_str(commit_b.to_hex(), COMMIT_2);

    let mut units = review_docs::coverage(&state).await.unwrap();
    units.push(review_docs::coverage_removed(&format!(
        "coverage:{CHANGE_C}"
    )));
    single_batch(units, 30, 20, &stab)
}

async fn generate_state_feed_config() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!(
        "review = \"after-land\"\n\
         collapse = \"{COLLAPSE_ROOT}\"\n\
         collapse_channel = true\n\
         [mirror]\n\
         remote = \"https://collapse.sh/example/project.git\"\n\
         [checks.pre_land]\n\
         commands = [\"cargo clippy --workspace --all-targets -- -D warnings\"]\n\
         [checks.post_land]\n\
         commands = [\"cargo test --workspace\"]\n"
    );
    let (state, _) = init_server(dir.path(), &cfg, &[]);
    let allow = [
        RelayKind::Approve,
        RelayKind::Land,
        RelayKind::Mark,
        RelayKind::Config,
    ];
    RelayStore::open(&state.config.data_dir)
        .unwrap()
        .set(&allow, FIXED_SECS as u64)
        .unwrap();

    let config_unit = review_docs::config(&state).await.unwrap().expect("config");
    let relay_unit = pass::relay_unit(&state.config.data_dir).unwrap();
    single_batch(
        vec![config_unit, relay_unit],
        40,
        30,
        &Stabilizer::default(),
    )
}

fn generate_state_feed_post_land() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!("review = \"after-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n");
    let (state, base) = init_server(dir.path(), &cfg, &[]);
    let events = post_land_events(base);
    let mut through = 0;
    for (kind, change, detail) in events {
        through = state
            .events
            .publish(kind, change.map(str::to_owned), detail);
    }
    let conn = metadata::open(&state.config.metadata_path()).unwrap();
    conn.execute(
        "UPDATE events SET created_at = ?1 + (seq * 1000)",
        [FIXED_MS],
    )
    .unwrap();
    drop(conn);

    let units = log_lines::read(&state.config.metadata_path(), 0, through, false).unwrap();
    single_batch(units, through, 0, &Stabilizer::default())
}

type EventSpec = (EventKind, Option<&'static str>, Value);

fn post_land_events(base: ObjectId) -> Vec<EventSpec> {
    let commit_1: ObjectId = COMMIT_1.parse().unwrap();
    let commit_2: ObjectId = COMMIT_2.parse().unwrap();
    let commit_3: ObjectId = COMMIT_3.parse().unwrap();
    let submitted = Submitted {
        by: "checker@agent-2".into(),
        at: FIXED_MS,
    };
    let culprit = BisectCulprit {
        commit: commit_2,
        parent: commit_1,
        change: CHANGE_B.into(),
        output: "test failed\n".into(),
    };
    let paths = vec!["a.txt".to_owned()];
    let mut events = vec![
        (
            EventKind::Conflict,
            Some(CHANGE_A),
            landing::conflict_routes::event_detail(&paths, base),
        ),
        (
            EventKind::Built,
            None,
            post_land::builds::event_detail(&commit_1, Some(&submitted)),
        ),
        (
            EventKind::Green,
            Some(CHANGE_A),
            post_land::green_detail(COMMIT_1, Some(&submitted.by), Some(submitted.at)),
        ),
        (
            EventKind::PostLandFailure,
            Some(CHANGE_B),
            post_land::failures::event_detail(
                1,
                &culprit,
                CheckKind::Test,
                false,
                Some(&submitted.by),
                Some(submitted.at),
            ),
        ),
    ];
    events.extend(revert_events(commit_2, commit_3, &paths));
    events.extend(bisect_events(commit_1, commit_2, &submitted, culprit));
    events.extend(mirror_events(commit_1));
    events
}

fn revert_events(commit_2: ObjectId, commit_3: ObjectId, paths: &[String]) -> Vec<EventSpec> {
    let refusal = LandError::RevertConflict {
        base: commit_3,
        paths: paths.to_vec(),
    };
    vec![
        (
            EventKind::Reverted,
            Some(CHANGE_B),
            landing::revert::event_detail(COMMIT_2, COMMIT_3, "build error\n"),
        ),
        (
            EventKind::RevertRefused,
            Some(CHANGE_B),
            landing::revert_refused::event_detail(
                commit_2,
                commit_3,
                &[commit_3],
                paths,
                &refusal.to_string(),
            ),
        ),
    ]
}

fn bisect_events(
    commit_1: ObjectId,
    commit_2: ObjectId,
    submitted: &Submitted,
    culprit: BisectCulprit,
) -> Vec<EventSpec> {
    let job = BisectJob {
        session: 1,
        step: 2,
        commit: commit_1,
        kind: CheckKind::Test,
    };
    let after = BisectState {
        id: 1,
        kind: CheckKind::Test,
        job: None,
        culprit: Some(culprit),
        flaky: false,
    };
    vec![
        (
            EventKind::Bisect,
            None,
            bisect::progress::opened_detail(
                1,
                CheckKind::Test,
                commit_2,
                Some(submitted),
                commit_1,
                1,
            ),
        ),
        (
            EventKind::Bisect,
            None,
            bisect::progress::accepted_detail(
                &job,
                true,
                Some(submitted),
                &after,
                bisect::planned_steps(1),
            ),
        ),
    ]
}

fn mirror_events(commit_1: ObjectId) -> Vec<EventSpec> {
    let push_error = mirror::SyncError::Push {
        remote: "https://secret/repo.git".into(),
        branch: "main".into(),
        commit: commit_1,
        failure: mirror::PushFailure {
            rejected: false,
            message: "connection refused".into(),
        },
    };
    let synced = mirror::Synced::Pushed {
        branch: "main".into(),
        commit: commit_1,
    };
    vec![
        (
            EventKind::Mirror,
            None,
            mirror::failure_detail(&push_error, 1, Duration::from_secs(1)),
        ),
        (
            EventKind::Mirror,
            None,
            mirror::recovered_detail(&synced, 1).expect("pushed mirror detail"),
        ),
    ]
}

fn seed_machines(data_dir: &Path, now: u64) {
    let conn = crate::machines::open(data_dir).unwrap();
    let rows = [
        (
            "agent-1",
            "workstation",
            "agent",
            "active",
            now - 25,
            now + 86_400 * 20,
        ),
        (
            "agent-2",
            "checker-box",
            "checker",
            "active",
            now - 600,
            now + 86_400 * 10,
        ),
        (
            "collapse",
            "Collapse action relay",
            "agent",
            "active",
            now - 60,
            0,
        ),
        ("pending-1", "new-runner", "agent", "pending", now - 300, 0),
        (
            "revoked-1",
            "old-laptop",
            "agent",
            "revoked",
            now - 3_600,
            0,
        ),
    ];
    for (id, name, role, status, contact, expires) in rows {
        conn.execute(
            "INSERT INTO machines (id, name, role, status, last_contact, credential_hash, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'secret-hash-never-sent', ?6)",
            rusqlite::params![id, name, role, status, contact, expires],
        )
        .unwrap();
    }
}

fn generate_state_feed_machines() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!("review = \"after-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n");
    let (state, _) = init_server(dir.path(), &cfg, &[]);
    let now = FIXED_SECS as u64;
    seed_machines(&state.config.data_dir, now);

    let mut running = crate::machines::manage::Presence::new();
    let mut agents = BTreeMap::new();
    agents.insert("alice".to_owned(), 1usize);
    agents.insert("bob".to_owned(), 1usize);
    running.insert("agent-1".to_owned(), agents);

    let unit = machines_doc::machines(&state.config.metadata_path(), &running, now).unwrap();
    single_batch(vec![unit], 50, 40, &Stabilizer::default())
}

fn generate_state_feed_presence() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!("review = \"after-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n");
    let (state, _) = init_server(dir.path(), &cfg, &[]);
    for (member, nick, machine) in [
        ("m-alice", "alice", "agent-1"),
        ("m-bob", "bob", "agent-2"),
        ("m-carol", "carol", "agent-1"),
    ] {
        let req = ChannelRequest::Join {
            member: member.into(),
            nick: nick.into(),
            machine: machine.into(),
        };
        channel::run(&state.events, req).unwrap();
    }
    channel::run(
        &state.events,
        ChannelRequest::SetMode {
            member: "m-bob".into(),
            mode: MessageMode::HeadsDown,
        },
    )
    .unwrap();
    channel::run(
        &state.events,
        ChannelRequest::Leave {
            member: "m-carol".into(),
        },
    )
    .unwrap();

    let conn = metadata::open(&state.config.metadata_path()).unwrap();
    conn.execute(
        "UPDATE channel_members SET joined_at = ?1 WHERE member = 'm-alice'",
        [FIXED_MS - 120_000],
    )
    .unwrap();
    conn.execute(
        "UPDATE channel_members SET joined_at = ?1 WHERE member = 'm-bob'",
        [FIXED_MS - 60_000],
    )
    .unwrap();
    drop(conn);

    let unit = machines_doc::presence(&state.config.metadata_path(), FIXED_MS, false).unwrap();
    single_batch(vec![unit], 60, 50, &Stabilizer::default())
}

fn record_channel_activity(state: &ServerState, stab: &mut Stabilizer) {
    let joined = channel::run(
        &state.events,
        ChannelRequest::Join {
            member: "m-alice".into(),
            nick: "alice".into(),
            machine: "agent-1".into(),
        },
    )
    .unwrap();
    if let ChannelAnswer::Member(member) = joined {
        stab.map_int(member.joined_at, FIXED_MS);
    }
    channel::run(
        &state.events,
        ChannelRequest::SetMode {
            member: "m-alice".into(),
            mode: MessageMode::HeadsDown,
        },
    )
    .unwrap();
    let claimed = channel::run(
        &state.events,
        ChannelRequest::ClaimPiece {
            member: "m-alice".into(),
            piece: Some("issue:436".into()),
            task: "Publish golden wire fixtures".into(),
            branch: "vel/issue-436".into(),
        },
    )
    .unwrap();
    if let ChannelAnswer::Piece(PieceOutcome::Granted { claim, .. }) = claimed {
        stab.map_int(claim.since, FIXED_MS);
    }
    let sent = channel::run(
        &state.events,
        ChannelRequest::Send(SendMessage {
            from: Author::Agent {
                member: "m-alice".into(),
            },
            to: vec!["alice".into()],
            intent: Intent::Inform,
            text: "Wire fixtures updated".into(),
        }),
    )
    .unwrap();
    if let ChannelAnswer::Sent(msg) = sent {
        stab.map_int(msg.time, FIXED_MS);
    }
}

fn generate_state_feed_channel() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!(
        "review = \"after-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\ncollapse_channel = true\n"
    );
    let (state, _) = init_server(dir.path(), &cfg, &[]);
    let mut stab = Stabilizer::default();
    record_channel_activity(&state, &mut stab);

    // Ledger entries must never appear in channel lines (R9).
    channel::run(
        &state.events,
        ChannelRequest::Append(AppendEntry {
            ledger: ChannelLedger::Discovery,
            author: Author::Agent {
                member: "m-alice".into(),
            },
            text: "never in channel feed".into(),
            velocity_issue: None,
        }),
    )
    .unwrap();
    let left = channel::run(
        &state.events,
        ChannelRequest::Leave {
            member: "m-alice".into(),
        },
    )
    .unwrap();
    if let ChannelAnswer::Member(member) = left {
        if let Some(at) = member.left_at {
            stab.map_int(at, FIXED_MS);
        }
    }

    let through = state.events.last_seq();
    let conn = metadata::open(&state.config.metadata_path()).unwrap();
    conn.execute(
        "UPDATE events SET created_at = ?1 + (seq * 1000)",
        [FIXED_MS],
    )
    .unwrap();
    drop(conn);

    let units: Vec<Unit> = log_lines::read(&state.config.metadata_path(), 0, through, true)
        .unwrap()
        .into_iter()
        .filter(|u| u.items.iter().all(|(s, _)| *s == Section::Channel))
        .collect();
    single_batch(units, through, 0, &stab)
}

#[derive(Default)]
struct CapturedHttp {
    requests: Vec<(String, String, Vec<u8>)>,
}

async fn spawn_capture(
    responder: impl Fn(&str, &str, &[u8]) -> (u16, String) + Send + Sync + 'static,
) -> (String, Arc<Mutex<CapturedHttp>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/example/project", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(CapturedHttp::default()));
    let store = Arc::clone(&captured);
    let responder = Arc::new(responder);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (method, path, body) = read_http_request(&mut socket).await;
            store
                .lock()
                .unwrap()
                .requests
                .push((method.clone(), path.clone(), body.clone()));
            let (status, reply_body) = responder(&method, &path, &body);
            let reply = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{reply_body}",
                reply_body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (root, captured)
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> (String, String, Vec<u8>) {
    let mut request = Vec::new();
    let mut buffer = [0u8; 65536];
    let (head_end, length) = loop {
        let read = socket.read(&mut buffer).await.unwrap();
        if read == 0 {
            break (request.len(), 0);
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse::<usize>().unwrap());
            break (end + 4, length);
        }
    };
    while request.len() < head_end + length {
        let read = socket.read(&mut buffer).await.unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let first = String::from_utf8_lossy(&request[..head_end]);
    let mut parts = first.lines().next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    (method, path, request[head_end..head_end + length].to_vec())
}

fn seed_ledger_entries(state: &ServerState) {
    channel::run(
        &state.events,
        ChannelRequest::Join {
            member: "m-alice".into(),
            nick: "alice".into(),
            machine: "agent-1".into(),
        },
    )
    .unwrap();
    let entries = [
        (
            ChannelLedger::Discovery,
            "Preflight lockfile check timed out",
            Some("Fix preflight timeout"),
        ),
        (
            ChannelLedger::PastWork,
            r#"velocity-airc/v1:{"kind":"checkout"}"#,
            None,
        ),
        (ChannelLedger::PastWork, "Landed golden wire fixtures", None),
        (
            ChannelLedger::Discovery,
            r#"velocity-airc/discovery/v1:{"text":"State feed envelope drifted","rosen_issue":"Publish golden wire fixtures"}"#,
            None,
        ),
        (
            ChannelLedger::PastWork,
            r#"{"kind":"task_result","ok":true}"#,
            None,
        ),
    ];
    for (ledger, text, velocity_issue) in entries {
        channel::run(
            &state.events,
            ChannelRequest::Append(AppendEntry {
                ledger,
                author: Author::Agent {
                    member: "m-alice".into(),
                },
                text: text.into(),
                velocity_issue: velocity_issue.map(str::to_owned),
            }),
        )
        .unwrap();
    }
    let conn = metadata::open(&state.config.metadata_path()).unwrap();
    conn.execute(
        "UPDATE channel_ledger SET created_at = ?1 + (seq * 1000)",
        [FIXED_MS],
    )
    .unwrap();
}

async fn generate_ledger_push() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let cfg = format!("review = \"after-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n");
    let (state, _) = init_server(dir.path(), &cfg, &[]);
    seed_ledger_entries(&state);

    let (root, captured) = spawn_capture(|_, _, body| {
        let parsed: Value = serde_json::from_slice(body).unwrap();
        let mut discovery = 0u64;
        let mut past_work = 0u64;
        for entry in parsed["entries"].as_array().unwrap() {
            let seq = entry["sequence"].as_u64().unwrap();
            match entry["ledger"].as_str().unwrap() {
                "discovery" => discovery = discovery.max(seq),
                "past_work" => past_work = past_work.max(seq),
                other => panic!("unexpected ledger {other}"),
            }
        }
        (
            200,
            json!({ "highest_sequence": { "discovery": discovery, "past_work": past_work } })
                .to_string(),
        )
    })
    .await;

    let endpoint = format!("{root}/ledgers");
    let mut session = LedgerSession::default();
    let pushed = ledger_push::push_to(&state.config, &endpoint, &mut session)
        .await
        .unwrap();
    assert!(matches!(
        pushed,
        ledger_push::Pushed::Current { sent: 3, .. }
    ));

    let reqs = captured.lock().unwrap();
    reqs.requests
        .iter()
        .find_map(|(_, _, body)| {
            let v: Value = serde_json::from_slice(body).unwrap();
            (!v["entries"].as_array().unwrap().is_empty()).then_some(v)
        })
        .expect("captured ledger push batch")
}

fn strip_repo_prefix(path: &str) -> String {
    path.strip_prefix("/example/project")
        .unwrap_or(path)
        .to_owned()
}

struct RelaySeed {
    state: Arc<ServerState>,
    id_open: ChangeId,
    id_landed: ChangeId,
    landed_commit: ObjectId,
    all_setting: RelaySetting,
    queued: Vec<Value>,
}

async fn seed_relay_server(dir: &Path) -> RelaySeed {
    let base_toml = format!("review = \"before-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n");
    let (state, _) = init_server(dir, &base_toml, &[("a.txt", b"initial\n")]);
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let base_blob = store.put_blob(base_toml.as_bytes()).unwrap();

    let id_open = new_change(&state, "alice").await;
    let (head_open, _) = write_and_name(&state, id_open, "a.txt", b"updated\n", "Change A").await;
    let id_landed = new_change(&state, "bob").await;
    let (head_landed, _) =
        write_and_name(&state, id_landed, "b.txt", b"landed\n", "Change B").await;
    approve_change(&state, id_landed, head_landed, "reviewer").await;
    let landed_commit = land_change(&state, id_landed).await;
    state.landing.settle();

    let conn = crate::machines::open(&state.config.data_dir).unwrap();
    conn.execute(
        "INSERT INTO machines (id, name, role, status, last_contact, credential_hash, expires_at)
         VALUES ('agent-lost', 'lost-laptop', 'agent', 'active', ?1, 'secret', ?2)",
        rusqlite::params![FIXED_SECS as u64 - 120, FIXED_SECS as u64 + 86_400],
    )
    .unwrap();
    drop(conn);

    let all_setting = RelayStore::open(&state.config.data_dir)
        .unwrap()
        .set(&RelayKind::ALL, FIXED_SECS as u64)
        .unwrap();
    let queued = relay_queued_actions(id_open, head_open, id_landed, landed_commit, base_blob);
    RelaySeed {
        state,
        id_open,
        id_landed,
        landed_commit,
        all_setting,
        queued,
    }
}

fn relay_queued_actions(
    id_open: ChangeId,
    head_open: ObjectId,
    id_landed: ChangeId,
    landed_commit: ObjectId,
    base_blob: ObjectId,
) -> Vec<Value> {
    let new_toml = format!("review = \"after-land\"\ncollapse = \"{COLLAPSE_ROOT}\"\n");
    let expires = FIXED_MS + 600_000;
    vec![
        json!({
            "id": 1, "kind": RelayKind::Approve.action_name(), "account": "owner",
            "expires_at": expires,
            "arguments": { "change": id_open, "head": head_open },
        }),
        json!({
            "id": 2, "kind": RelayKind::Land.action_name(), "account": "owner",
            "expires_at": expires,
            "arguments": { "change": id_open },
        }),
        json!({
            "id": 3, "kind": RelayKind::Mark.action_name(), "account": "owner",
            "expires_at": expires,
            "arguments": {
                "change": id_landed,
                "commit": landed_commit,
                "state": ReviewedState::Reviewed.as_str(),
            },
        }),
        json!({
            "id": 4, "kind": RelayKind::Config.action_name(), "account": "owner",
            "expires_at": expires,
            "arguments": {
                "base": landed_commit,
                "expected_blob": base_blob,
                "text": new_toml,
                "message": "rosen.toml: switch to after-land review",
            },
        }),
        json!({
            "id": 5, "kind": RelayKind::Revoke.action_name(), "account": "owner",
            "expires_at": expires,
            "arguments": { "machine": "agent-lost" },
        }),
    ]
}

async fn run_relay_posts(
    seed: &RelaySeed,
    remote: &Remote,
    fetched_raw: &[Value],
    stab: &mut Stabilizer,
) -> Vec<String> {
    let mut posted_kinds = Vec::new();
    for raw in fetched_raw {
        let parsed = Fetched::parse(raw).expect("valid relay action");
        let checked = action::check(&parsed, &seed.all_setting, FIXED_MS).expect("allowed action");
        let done = action::run(&seed.state, checked).await;
        if parsed.kind == RelayKind::Land.action_name() {
            seed.state.landing.settle();
        }
        capture_relay_dynamic(&parsed.kind, &done, stab);
        remote
            .post_result(parsed.id, done.state, &done.result.to_string())
            .await
            .unwrap();
        posted_kinds.push(parsed.kind);
    }
    let refused = Fetched::parse(&json!({
        "id": 6, "kind": RelayKind::Revoke.action_name(), "account": "owner",
        "expires_at": FIXED_MS + 600_000, "arguments": { "machine": "ghost" },
    }))
    .unwrap();
    let refused_call = action::check(&refused, &seed.all_setting, FIXED_MS).unwrap();
    let refused_done = action::run(&seed.state, refused_call).await;
    remote
        .post_result(6, refused_done.state, &refused_done.result.to_string())
        .await
        .unwrap();
    posted_kinds.push("machine_revoke_refused".to_owned());
    posted_kinds
}

async fn generate_relay_fixtures() -> (Value, Value, Value) {
    let dir = tempfile::tempdir().unwrap();
    let seed = seed_relay_server(dir.path()).await;
    let mut stab = Stabilizer::default();
    let doorbell_body = json!({ "generation": 5 });
    let actions_body = json!({ "actions": seed.queued });
    let (doorbell_reply, actions_reply) = (doorbell_body.to_string(), actions_body.to_string());
    let (root, captured) = spawn_capture(move |_, path, _| {
        if path.ends_with("/rosen/doorbell") {
            return (200, doorbell_reply.clone());
        }
        if path.contains("/rosen/actions?after=") {
            return (200, actions_reply.clone());
        }
        (200, "{}".to_owned())
    })
    .await;

    let remote = Remote {
        root,
        credential: None,
        scratch: dir.path().join("relay-scratch"),
    };
    assert_eq!(remote.doorbell().await.unwrap(), 5);
    let fetched_raw = remote.actions(0).await.unwrap();
    assert_eq!(fetched_raw.len(), 5);
    let posted_kinds = run_relay_posts(&seed, &remote, &fetched_raw, &mut stab).await;

    stab.map_str(seed.id_open.to_string(), CHANGE_A);
    stab.map_str(seed.id_landed.to_string(), CHANGE_B);
    stab.map_str(seed.landed_commit.to_hex(), COMMIT_1);

    let reqs = captured.lock().unwrap();
    let (dm, dp, _) = &reqs.requests[0];
    let mut doorbell = json!({
        "method": dm, "path": strip_repo_prefix(dp), "response": doorbell_body,
    });
    let (am, ap, _) = &reqs.requests[1];
    let mut actions = json!({
        "method": am, "path": strip_repo_prefix(ap), "response": { "actions": fetched_raw },
    });
    let posts: Vec<Value> = posted_kinds
        .iter()
        .zip(reqs.requests.iter().skip(2))
        .map(|(kind, (method, path, body))| {
            let body: Value = serde_json::from_slice(body).unwrap();
            json!({ "kind": kind, "method": method, "path": strip_repo_prefix(path), "body": body })
        })
        .collect();
    let mut result = json!({ "posts": posts });
    stab.apply(&mut doorbell);
    stab.apply(&mut actions);
    stab.apply(&mut result);
    (doorbell, actions, result)
}

fn capture_relay_dynamic(kind: &str, done: &action::Done, stab: &mut Stabilizer) {
    match kind {
        "approve" => {
            if let Some(ts) = done.result["created_at"].as_i64() {
                stab.map_int(ts, FIXED_MS);
            }
        }
        "land" => {
            if let Some(commit) = done.result["outcome"]["commit"].as_str() {
                stab.map_str(commit, COMMIT_2);
            }
        }
        "mark" => {
            if let Some(ts) = done.result["updated_at"].as_i64() {
                stab.map_int(ts, FIXED_MS);
            }
        }
        "config_change" => {
            if let Some(change) = done.result["change"].as_str() {
                stab.map_str(change, CHANGE_C);
            }
            if let Some(commit) = done.result["commit"].as_str() {
                stab.map_str(commit, COMMIT_3);
            }
        }
        _ => {}
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn collapse_wire_fixtures_match_real_senders() {
    let (doorbell, actions, result) = generate_relay_fixtures().await;
    let fixtures = [
        ("state-feed-change.json", generate_state_feed_change().await),
        ("state-feed-review.json", generate_state_feed_review().await),
        (
            "state-feed-coverage.json",
            generate_state_feed_coverage().await,
        ),
        ("state-feed-config.json", generate_state_feed_config().await),
        ("state-feed-post-land.json", generate_state_feed_post_land()),
        ("state-feed-machines.json", generate_state_feed_machines()),
        ("state-feed-presence.json", generate_state_feed_presence()),
        ("state-feed-channel.json", generate_state_feed_channel()),
        ("ledger-push.json", generate_ledger_push().await),
        ("relay-doorbell.json", doorbell),
        ("relay-actions.json", actions),
        ("relay-result.json", result),
    ];
    let mut failures = Vec::new();
    for (name, value) in fixtures {
        if let Some(failure) = check_fixture(name, &value) {
            failures.push(failure);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
