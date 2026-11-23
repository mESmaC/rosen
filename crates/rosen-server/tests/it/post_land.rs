//! Risk (R12): a stale checker must not green future lands or regress the
//! durable green prefix; a batch must notify every covered change.
use rosen_core::{AgentId, Commit, ObjectId, Signature, Tree};
use rosen_protocol::*;
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

fn setup() -> (tempfile::TempDir, Arc<ServerState>) {
    setup_files(&[])
}

fn setup_files(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Arc<ServerState>) {
    let temp = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: temp.path().into(),
        endpoint: Endpoint::new("green-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let entries = files
        .iter()
        .map(|(path, bytes)| {
            rosen_core::TreeEntry::new(
                rosen_core::FileMode::Regular,
                *path,
                store.put_blob(bytes).unwrap(),
            )
            .unwrap()
        })
        .collect();
    let root = store
        .put_commit(&Commit {
            tree: store.put_tree(&Tree::new(entries).unwrap()).unwrap(),
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
    (temp, state)
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        other => panic!("{other:?}"),
    }
}

async fn next(state: &Arc<ServerState>) -> PostLandWork {
    match call(state, Request::PostLandNext).await {
        Response::PostLand(work) => work,
        other => panic!("{other:?}"),
    }
}

async fn land(state: &Arc<ServerState>, n: u8) -> ObjectId {
    let (change, head) = new_edit(state, &format!("file{n}.rs"), &[n]).await;
    approve_and_receipt(state, &change, head, current_base(state)).await;
    land_named(state, change).await
}

async fn new_edit(state: &Arc<ServerState>, path: &str, bytes: &[u8]) -> (String, ObjectId) {
    let create = NewChange {
        owner: AgentId::new("owner").unwrap(),
        on: None,
    };
    let Response::Change(record) = call(state, Request::ChangeNew(create)).await else {
        panic!()
    };
    let change = record.change.id.to_string();
    let write = WorkspaceWrite {
        change: change.clone(),
        path: path.into(),
        data: Some(WorkspaceBytes(bytes.into())),
    };
    call(state, Request::WorkspaceWrite(write)).await;
    let show = Request::ChangeShow {
        change: change.clone(),
    };
    let Response::Change(record) = call(state, show).await else {
        panic!()
    };
    (change, record.change.head)
}

fn current_base(state: &ServerState) -> ObjectId {
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .project_base()
        .unwrap()
        .unwrap()
        .1
}

async fn approve_and_receipt(
    state: &Arc<ServerState>,
    change: &str,
    head: ObjectId,
    base: ObjectId,
) {
    let receipt = SubmitReceipt {
        change: change.into(),
        head,
        base,
        check_set: "pre-land".into(),
        submitter: AgentId::new("checker").unwrap(),
        signature: None,
    };
    call(state, Request::ReceiptSubmit(receipt)).await;
    let approve = ApproveRequest {
        change: change.into(),
        head,
        reviewer: AgentId::new("reviewer").unwrap(),
    };
    call(state, Request::ReviewApprove(approve)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_green_survives_restart_and_excludes_future_lands() {
    let (_temp, state) = setup();
    assert!(next(&state).await.job.is_none());
    let a = land(&state, 1).await;
    let old = next(&state).await.job.unwrap();
    assert_eq!(old.commit, a);
    let b = land(&state, 2).await;
    let batch = next(&state).await.job.unwrap();
    let c = land(&state, 3).await;
    let mut wrong = batch.clone();
    wrong.check_set.commands.push("skip tests".into());
    assert!(matches!(
        dispatch(&state, Request::PostLandPassed(wrong)).await,
        Outcome::Error(_)
    ));
    assert_eq!(next(&state).await.last_green, None);
    call(&state, Request::PostLandPassed(batch.clone())).await;
    call(&state, Request::PostLandPassed(old)).await;
    call(&state, Request::PostLandPassed(batch)).await;
    let config = state.config.clone();
    drop(state);
    let restarted = ServerState::new(config);
    let work = next(&restarted).await;
    assert_eq!(work.last_green, Some(b));
    assert_eq!(work.job.unwrap().commit, c);
    let events = restarted
        .events
        .page(0, Some(&AgentId::new("owner").unwrap()), 100)
        .unwrap();
    let greens: Vec<_> = events
        .events
        .iter()
        .filter(|e| e.kind == EventKind::Green)
        .collect();
    assert_eq!(greens.len(), 2);
    assert_eq!(greens[0].detail["commit"], a.to_string());
    assert_eq!(greens[1].detail["commit"], b.to_string());
}

// Risk (R12): the executable SQL prefix update must agree with the proven
// model on first, repeated and out-of-order passes.
#[tokio::test]
async fn sql_prefix_matches_lean_fixtures() {
    use rosen_server::post_land::store;
    let fixture = include_str!("../../../../proofs/fixtures/batch-green.tsv");
    let mut platform_rows = 0usize;
    for line in fixture.lines().skip(1) {
        if let Some(offer) = line.strip_prefix("offer\t") {
            check_build_offer(offer).await;
            continue;
        }
        if let Some(platform) = line.strip_prefix("platform\t") {
            check_platform_prefix(platform).await;
            platform_rows += 1;
            continue;
        }
        let cells: Vec<u32> = line.split('\t').map(|n| n.parse().unwrap()).collect();
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        rosen_server::metadata::ensure_schema(&conn).unwrap();
        store::ensure_schema(&conn).unwrap();
        for n in 1..=3 {
            conn.execute(
                "INSERT INTO base_history(seq,ref_name,commit_id,change_id,tier)
                 VALUES (?1,'main',?2,?3,'normal')",
                rusqlite::params![n, format!("{n:040x}"), format!("change{n}")],
            )
            .unwrap();
        }
        let commit = |n: u32| format!("{n:040x}").parse::<ObjectId>().unwrap();
        if cells[0] > 0 {
            assert!(store::passed(&mut conn, "main", &commit(cells[0])).unwrap());
        }
        assert!(store::passed(&mut conn, "main", &commit(cells[1])).unwrap());
        assert_eq!(
            store::last_green(&conn, "main").unwrap(),
            Some(commit(cells[2]))
        );
        let pending = store::pending(&conn).unwrap();
        assert_eq!(pending.len(), cells[2] as usize);
    }
    assert!(
        platform_rows > 0,
        "expected platform rows in batch-green.tsv"
    );
}

fn platform_config(required: &[&str]) -> String {
    let req_toml = required
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut toml = format!(
        "review = \"after-land\"\n[checks.post_land]\nplatforms = [{req_toml}]\n\
         commands = [\"test\"]\n[checks.post_land.platform_commands]\n"
    );
    for plat in ["windows", "macos", "linux"] {
        if !required.contains(&plat) {
            toml.push_str(&format!("{plat} = [\"advisory-{plat}\"]\n"));
        }
    }
    toml
}

async fn platform_job_templates(
    state: &Arc<ServerState>,
) -> std::collections::BTreeMap<&'static str, PostLandJob> {
    let mut templates = std::collections::BTreeMap::new();
    for (plat, checker) in [
        ("windows", "windows-x86_64"),
        ("macos", "macos-arm64"),
        ("linux", "linux-x86_64"),
    ] {
        let Response::PostLand(work) = call(
            state,
            Request::PostLandNextFor {
                platform: checker.into(),
                kind: CheckKind::Test,
            },
        )
        .await
        else {
            panic!("expected post-land work for {plat}");
        };
        templates.insert(plat, work.job.expect("pending job"));
    }
    templates
}

// R12 (#440, #461): aggregate base green advances only to the minimum passing
// commit across all required platforms; advisory platforms never gate it.
async fn check_platform_prefix(row: &str) {
    let cells: Vec<&str> = row.split('\t').collect();
    assert_eq!(cells.len(), 4, "bad platform row: {row}");
    let required: Vec<&str> = cells[0].split(',').collect();
    let toml = platform_config(&required);
    let (_temp, state) = setup_files(&[("rosen.toml", toml.as_bytes())]);
    let commits = [
        land(&state, 1).await,
        land(&state, 2).await,
        land(&state, 3).await,
    ];
    let templates = platform_job_templates(&state).await;
    for entry in cells[1].split(',') {
        let (plat, pos) = entry.split_once(':').unwrap();
        let idx: usize = pos.parse().unwrap();
        let mut job = templates[plat].clone();
        job.commit = commits[idx - 1];
        call(&state, Request::PostLandPassed(job)).await;
    }
    let expected = |pos: usize| (pos > 0).then(|| commits[pos - 1]);
    let expected_agg: usize = cells[2].parse().unwrap();
    let Response::PostLandStatus(status) = call(&state, Request::PostLandStatus).await else {
        panic!("expected post-land status");
    };
    assert_eq!(
        status.last_green.map(|c| c.commit),
        expected(expected_agg),
        "aggregate mismatch for {row}"
    );
    let conn = rosen_server::metadata::open(&state.config.metadata_path()).unwrap();
    for entry in cells[3].split(',') {
        let (plat, pos) = entry.split_once(':').unwrap();
        let idx: usize = pos.parse().unwrap();
        let actual =
            rosen_server::post_land::last_green_for(&conn, "main", CheckKind::Test, Some(plat))
                .unwrap();
        assert_eq!(actual, expected(idx), "platform {plat} mismatch for {row}");
    }
}

// R1/R12: an adoption can be the tip without being eligible for build work.
async fn check_build_offer(row: &str) {
    let (mirrors, expected) = row.split_once('\t').unwrap();
    let (_temp, state) = setup();
    let path = state.config.metadata_path();
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let conn = rosen_server::metadata::open(&path).unwrap();
    let mut tip = current_base(&state);
    let mut rows = Vec::new();
    for pos in ["1", "2", "3"] {
        let mut commit = store.read_commit(&tip).unwrap();
        commit.parents = vec![tip];
        commit.message = format!("fixture row {pos}\n").into_bytes();
        tip = store.put_commit(&commit).unwrap();
        let (tier, change) = if mirrors.split(',').any(|mirror| mirror == pos) {
            ("mirror", None)
        } else {
            ("normal", Some(format!("change-{pos}")))
        };
        conn.execute(
            "INSERT INTO base_history(ref_name,commit_id,change_id,tier,post_land_done)
             VALUES ('main',?1,?2,?3,1)",
            rusqlite::params![tip.to_string(), change, tier],
        )
        .unwrap();
        rows.push((pos, tip));
    }
    conn.execute(
        "UPDATE base_refs SET target=?1 WHERE name='main'",
        [tip.to_string()],
    )
    .unwrap();
    let Response::PostLand(work) = call(&state, Request::PostLandBuildNext).await else {
        panic!("expected post-land work");
    };
    let expected = rows
        .iter()
        .find(|(pos, _)| *pos == expected)
        .map(|(_, commit)| *commit);
    assert_eq!(work.job.map(|job| job.commit), expected, "{row}");
}

// R3/R12 risk: tests must not postpone merge-mode builds or green their prefix;
// an unbuilt tip must not spread into new workspaces.
#[tokio::test(flavor = "multi_thread")]
async fn merge_mode_prioritizes_builds_and_starts_from_last_built() {
    let (_temp, state) = setup_files(&[("rosen.toml", b"recheck = 'merge'\n")]);
    let a = land(&state, 1).await;
    let old = next(&state).await.job.unwrap();
    assert_eq!(
        (old.kind, old.priority.as_str()),
        (CheckKind::Build, "landing")
    );
    call(&state, Request::PostLandPassed(old.clone())).await;
    let b = land(&state, 2).await;
    let Response::Change(fresh) = call(
        &state,
        Request::ChangeNew(NewChange {
            owner: AgentId::new("fresh").unwrap(),
            on: None,
        }),
    )
    .await
    else {
        panic!()
    };
    assert_eq!(fresh.change.parent, rosen_core::ChangeParent::Base(a));
    let build = next(&state).await.job.unwrap();
    assert_eq!(build.commit, b);
    call(&state, Request::PostLandPassed(build)).await;
    call(&state, Request::PostLandPassed(old)).await;
    let tests = next(&state).await;
    assert_eq!(tests.last_green, None);
    assert_eq!(tests.job.unwrap().kind, CheckKind::Test);
    let Response::Config(config) = call(&state, Request::ConfigShow(ConfigQuery::default())).await
    else {
        panic!()
    };
    assert_eq!(config.workspace_base, Some(b));
}

// Risk (R12, #441): a green range touching a deploy step's paths must emit
// `deploy_due` once per range, none for docs-only lands, run steps one at a
// time in order, skip pending steps covered by a newer green, and record
// `deploy_result` from checkers.
#[tokio::test(flavor = "multi_thread")]
async fn deploy_steps_gate_on_paths_serialize_and_skip_superseded() {
    let config = b"[deploy.server]\ncommand = \"deploy-server\"\npaths = [\"server/**\"]\n\
        secrets = [\"DEPLOY_TOKEN\"]\nplatform = \"linux\"\nplatforms = [\"linux\"]\n\
        [deploy.web]\ncommand = \"deploy-web\"\npaths = [\"web/**\"]\n";
    let (_temp, state) = setup_files(&[("rosen.toml", config)]);
    assert_deploy_config(&state, &[("server", Some("linux")), ("web", None)]).await;

    // Docs-only land goes green: no deploy_due.
    let docs_commit = land_path(&state, "docs/readme.md", b"hello").await;
    pass_next(&state, docs_commit).await;
    assert!(deploy_events(&state, EventKind::DeployDue).is_empty());

    // Land touching server/ and web/ goes green: only `server` is due first.
    let c1 = land_paths(&state, &[("server/app.rs", b"v1"), ("web/index.js", b"v1")]).await;
    pass_next(&state, c1).await;
    let dues = deploy_events(&state, EventKind::DeployDue);
    assert_eq!(dues.len(), 1);
    assert_eq!(dues[0].detail["step"], "server");
    assert_eq!(dues[0].detail["commit"], c1.to_string());
    assert_eq!(dues[0].detail["platform"], "linux");
    assert_eq!(
        dues[0].detail["secrets"],
        serde_json::json!(["DEPLOY_TOKEN"])
    );
    let server_id = dues[0].detail["id"].as_i64().unwrap();

    // Newer green touching web/ skips pending (c1, web) while server is running.
    let c2 = land_path(&state, "web/index.js", b"v2").await;
    pass_next(&state, c2).await;
    assert_eq!(deploy_events(&state, EventKind::DeployDue).len(), 1);

    // Reporting server result records deploy_result and dispatches (c2, web).
    let submit = SubmitDeployResult {
        id: server_id,
        step: "server".into(),
        commit: c1,
        status: DeployStatus::Passed,
        output: "deployed".into(),
        submitter: Some(AgentId::new("deployer").unwrap()),
    };
    let Response::DeployRecorded(rec) = call(&state, Request::DeployResult(submit.clone())).await
    else {
        panic!()
    };
    assert_eq!(rec.status, DeployStatus::Passed);
    assert_eq!(rec.submitter.as_deref(), Some("deployer@server"));
    call(&state, Request::DeployResult(submit)).await;
    assert_eq!(deploy_events(&state, EventKind::DeployResult).len(), 1);

    let dues = deploy_events(&state, EventKind::DeployDue);
    assert_eq!(dues.len(), 2);
    assert_eq!(dues[1].detail["step"], "web");
    assert_eq!(dues[1].detail["commit"], c2.to_string());
    assert_eq!(dues[1].detail["from"], docs_commit.to_string());
    assert!(dues[1].detail.get("platform").is_none());
}

async fn assert_deploy_config(state: &Arc<ServerState>, expected: &[(&str, Option<&str>)]) {
    let Response::Config(shown) = call(state, Request::ConfigShow(ConfigQuery::default())).await
    else {
        panic!()
    };
    let got: Vec<(&str, Option<&str>)> = shown
        .config
        .deploy
        .iter()
        .map(|s| (s.name.as_str(), s.platform.as_deref()))
        .collect();
    assert_eq!(got, expected);
}

async fn pass_next(state: &Arc<ServerState>, expected_commit: ObjectId) {
    let job = next(state).await.job.unwrap();
    assert_eq!(job.commit, expected_commit);
    call(state, Request::PostLandPassed(job)).await;
}

fn deploy_events(state: &ServerState, kind: EventKind) -> Vec<Event> {
    state
        .events
        .after(0)
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

async fn land_path(state: &Arc<ServerState>, path: &str, bytes: &[u8]) -> ObjectId {
    land_paths(state, &[(path, bytes)]).await
}

async fn land_paths(state: &Arc<ServerState>, files: &[(&str, &[u8])]) -> ObjectId {
    let create = NewChange {
        owner: AgentId::new("owner").unwrap(),
        on: None,
    };
    let Response::Change(record) = call(state, Request::ChangeNew(create)).await else {
        panic!()
    };
    let change = record.change.id.to_string();
    for (path, bytes) in files {
        let write = WorkspaceWrite {
            change: change.clone(),
            path: (*path).into(),
            data: Some(WorkspaceBytes((*bytes).into())),
        };
        call(state, Request::WorkspaceWrite(write)).await;
    }
    let Response::Change(record) = call(
        state,
        Request::ChangeShow {
            change: change.clone(),
        },
    )
    .await
    else {
        panic!()
    };
    approve_and_receipt(state, &change, record.change.head, current_base(state)).await;
    land_named(state, change).await
}

async fn land_named(state: &Arc<ServerState>, change: String) -> ObjectId {
    let Response::Land(answer) = call(state, Request::Land { change }).await else {
        panic!()
    };
    let LandStatus::Landed { commit, .. } = answer.outcome else {
        panic!("{answer:?}")
    };
    state.landing.settle();
    commit
}
