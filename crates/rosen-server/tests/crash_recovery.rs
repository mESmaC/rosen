//! Risk (SPEC R8 "Accept when"): a server crash mid-landing loses the land,
//! applies or announces it twice, or lets a replayed post-land run move a
//! change behind a base it already reached. Checks the real server against
//! `proofs/fixtures/crash-recovery.tsv`. A crash is a SQLite trigger abort at
//! a durable boundary, which leaves what a process death there would; a
//! restart is a fresh server state on the same data directory.

use rosen_core::{AgentId, Commit, ObjectId, Signature, Tree};
use rosen_protocol::Request::{self, ClaimPaths, Land, ReceiptSubmit, ReviewApprove};
use rosen_protocol::{ApproveRequest as Approve, ClaimRequest, LandStatus, Outcome, Response};
use rosen_protocol::{NewChange, SubmitReceipt, WorkspaceWrite};
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use rusqlite::Connection;
use std::sync::Arc;

type State = Arc<ServerState>;

fn start(data_dir: &std::path::Path) -> State {
    let endpoint = rosen_protocol::Endpoint::new("crash-recovery");
    ServerState::new(ServerConfig {
        data_dir: data_dir.into(),
        endpoint,
        warm_pool_dir: None,
    })
}

async fn ok(state: &State, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        Outcome::Error(body) => panic!("{body}"),
    }
}

/// The first column of the first row, as text.
fn read(db: &Connection, query: &str) -> String {
    let query = format!("SELECT CAST(({query}) AS TEXT)");
    db.query_row(&query, [], |row| row.get(0)).unwrap()
}

/// A new change that writes `path`.
async fn change(state: &State, path: &str) -> String {
    let owner = AgentId::new("agent").unwrap();
    let Response::Change(made) = ok(state, Request::ChangeNew(NewChange { owner, on: None })).await
    else {
        panic!("change new")
    };
    let (change, data) = (
        made.change.id.to_string(),
        Some(rosen_protocol::WorkspaceBytes(path.into())),
    );
    let write = WorkspaceWrite {
        change: change.clone(),
        path: path.into(),
        data,
    };
    ok(state, Request::WorkspaceWrite(write)).await;
    change
}

/// Approves the change and submits a receipt on the current base.
async fn ready(state: &State, db: &Connection, change: String) {
    let id = |query: &str| ObjectId::from_hex(&read(db, query)).unwrap();
    let head = id(&format!("SELECT head FROM changes WHERE id = '{change}'"));
    let (base, reviewer) = (
        id("SELECT target FROM base_refs"),
        AgentId::new("r").unwrap(),
    );
    let submitter = reviewer.clone();
    ok(
        state,
        ReviewApprove(Approve {
            change: change.clone(),
            reviewer,
            head,
        }),
    )
    .await;
    let check_set = "pre-land".into();
    let receipt = SubmitReceipt {
        change,
        head,
        base,
        check_set,
        submitter,
        signature: None,
    };
    ok(state, ReceiptSubmit(receipt)).await;
}

/// The land whose commit the overlapping change `x` sits on: base history
/// positions 2, 3 and 4 are lands `a`, `b` and `c`.
fn target(db: &Connection, x: &str) -> String {
    let seq = read(db, &format!("SELECT h.seq FROM changes c JOIN base_history h ON h.commit_id = c.parent_base WHERE c.id = '{x}'"));
    ["a", "b", "c"][seq.parse::<usize>().unwrap() - 2].into()
}

fn root(state: &State) {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let (tree, author) = (
        store.put_tree(&Tree::default()).unwrap(),
        Signature::new("t", "t@t", 0),
    );
    let (committer, parents, extra_headers, message) = (author.clone(), vec![], vec![], vec![]);
    let commit = Commit {
        tree,
        parents,
        author,
        committer,
        extra_headers,
        message,
    };
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    ledger
        .init_base("main", &store.put_commit(&commit).unwrap())
        .unwrap();
}

/// Runs one fixture row and returns its output columns.
async fn scenario(a_pending: bool, crash: u32) -> Vec<String> {
    let temp = tempfile::tempdir().unwrap();
    let first = start(temp.path());
    root(&first);
    let db = Connection::open(first.config.metadata_path()).unwrap();
    // X claims every file a, b and c write, so each land overlaps it.
    let x = change(&first, "x").await;
    let paths = vec!["s/**".into()];
    ok(
        &first,
        ClaimPaths(ClaimRequest {
            change: x.clone(),
            paths,
        }),
    )
    .await;
    let (a, b) = (change(&first, "s/a").await, change(&first, "s/b").await);
    let c = change(&first, "s/c").await;
    for change in [a.clone(), b] {
        ready(&first, &db, change.clone()).await;
        ok(&first, Land { change }).await;
    }
    // A land answers before its post-land work runs (issue #190).
    first.landing.settle();
    let unfinish =
        |id| format!("UPDATE base_history SET post_land_done = 0 WHERE change_id = '{id}'");
    if a_pending {
        db.execute_batch(&unfinish(&a)).unwrap();
    }
    ready(&first, &db, c.clone()).await;
    // Crash 0 dies inside the land transaction; crash 1 right after it commits.
    let tables = [
        "base_history WHEN NEW.change_id",
        "landing_overlap_batches WHEN NEW.landed",
    ];
    if let Some(on) = tables.get(crash as usize) {
        db.execute_batch(&format!("CREATE TRIGGER crash BEFORE INSERT ON {on} = '{c}' BEGIN SELECT RAISE(ABORT, 'crash'); END")).unwrap();
    }
    let outcome = dispatch(&first, Land { change: c.clone() }).await;
    first.landing.settle();
    assert_eq!(
        matches!(outcome, Outcome::Error(_)),
        crash == 0,
        "{outcome:?}"
    );
    db.execute_batch("DROP TRIGGER IF EXISTS crash").unwrap();
    if crash == 2 {
        db.execute_batch(&unfinish(&c)).unwrap();
    }
    let restarted = start(temp.path());
    restarted.landing.recover(&restarted).unwrap();
    let after_recovery = target(&db, &x);
    let Response::Land(answer) = ok(&restarted, Land { change: c.clone() }).await else {
        panic!()
    };
    restarted.landing.settle();
    let retry = match answer.outcome {
        LandStatus::Landed { .. } => "landed",
        LandStatus::AlreadyLanded { .. } => "already_landed",
        refused => panic!("{refused:?}"),
    };
    let count = |table: &str, filter: &str| {
        read(&db, &format!("SELECT COUNT(*) FROM {table} WHERE {filter}"))
    };
    // The replay must not announce a land its crashed run already announced.
    assert_eq!(
        count("events", &format!("kind = 'landed' AND change_id = '{c}'")),
        "1"
    );
    let landed = count("base_history", &format!("change_id = '{c}'"));
    let pending = count(
        "base_history",
        "change_id IS NOT NULL AND post_land_done = 0",
    );
    vec![
        after_recovery,
        retry.into(),
        landed,
        pending,
        target(&db, &x),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_at_every_land_step_matches_the_recovery_model() {
    let table = include_str!("../../../proofs/fixtures/crash-recovery.tsv");
    for row in table.lines().skip(1).filter(|line| !line.trim().is_empty()) {
        let cells: Vec<&str> = row.trim_end().split('\t').collect();
        let actual = scenario(cells[0] == "1", cells[1].parse().unwrap()).await;
        assert_eq!(actual, cells[2..], "fixture row {row}");
    }
}
