//! Risk (SPEC R8 "Accept when"): a server crash mid-landing loses the land,
//! applies or announces it twice, or lets a replayed post-land run move a
//! change behind a base it already reached. Checks the real server against
//! `proofs/fixtures/crash-recovery.tsv`. A crash is a SQLite trigger abort at
//! a durable boundary, which leaves what a process death there would; a
//! restart is a fresh server state on the same data directory.

use rosen_core::{AgentId, Commit, ObjectId, Signature, Tree};
use rosen_protocol::Request::{self, Land, ReceiptSubmit, ReviewApprove};
use rosen_protocol::{ApproveRequest as Approve, LandStatus, Outcome, Response};
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
async fn change(state: &State, path: &str, on: Option<String>) -> String {
    let owner = AgentId::new("agent").unwrap();
    let Response::Change(made) = ok(state, Request::ChangeNew(NewChange { owner, on })).await
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

/// Before detachment, the child's basis is its unlanded parent's base.
fn child_base(db: &Connection, child: &str) -> String {
    let seq = read(
        db,
        &format!(
            "SELECT h.seq FROM changes c
         LEFT JOIN changes p ON p.id = c.parent_change
         JOIN base_history h ON h.commit_id = COALESCE(c.parent_base, p.parent_base)
         WHERE c.id = '{child}'"
        ),
    );
    match seq.as_str() {
        "1" => "root",
        "2" => "a",
        "3" => "b",
        "4" => "c",
        _ => panic!("unexpected child base position: {seq}"),
    }
    .into()
}

/// Crashes bracket the child's durable restack, independently of overlap probes.
fn inject_crash(db: &Connection, crash: u32, c: &str, child: &str) {
    let landed = format!("(SELECT commit_id FROM base_history WHERE change_id = '{c}')");
    let (operation, table, when) = match crash {
        0 => ("INSERT", "base_history", format!("NEW.change_id = '{c}'")),
        1 => (
            "UPDATE",
            "changes",
            format!("NEW.id = '{child}' AND NEW.parent_base = {landed}"),
        ),
        2 => (
            "UPDATE",
            "base_history",
            format!("OLD.change_id = '{c}' AND NEW.post_land_done = 1"),
        ),
        _ => return,
    };
    let sql = format!(
        "CREATE TRIGGER crash BEFORE {operation} ON {table} WHEN {when}
         BEGIN SELECT RAISE(ABORT, 'crash'); END"
    );
    db.execute_batch(&sql).unwrap();
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

async fn prepare(first: &State, db: &Connection, a_pending: bool) -> (String, String) {
    let a = change(first, "s/a", None).await;
    let b = change(first, "s/b", None).await;
    for change in [a.clone(), b] {
        ready(first, db, change.clone()).await;
        ok(first, Land { change }).await;
    }
    first.landing.settle();
    if a_pending {
        db.execute(
            "UPDATE base_history SET post_land_done = 0 WHERE change_id = ?1",
            [a],
        )
        .unwrap();
    }
    // The child inherits b through c, then c's land must detach and advance it.
    let c = change(first, "s/c", None).await;
    let child = change(first, "x", Some(c.clone())).await;
    ready(first, db, c.clone()).await;
    (c, child)
}

async fn crash_land(first: &State, db: &Connection, c: &str, child: &str, crash: u32) {
    inject_crash(db, crash, c, child);
    let outcome = dispatch(first, Land { change: c.into() }).await;
    first.landing.settle();
    assert_eq!(
        matches!(outcome, Outcome::Error(_)),
        crash == 0,
        "{outcome:?}"
    );
    // Both durable boundaries must interrupt real work, not simulate a pending flag.
    let unfinished = format!("change_id = '{c}' AND post_land_done = 0");
    let unfinished = read(
        db,
        &format!("SELECT COUNT(*) FROM base_history WHERE {unfinished}"),
    );
    assert_eq!(unfinished, if matches!(crash, 1 | 2) { "1" } else { "0" });
    db.execute_batch("DROP TRIGGER IF EXISTS crash").unwrap();
}

/// Runs one fixture row against a real D19 child, including an older pending land.
async fn scenario(a_pending: bool, crash: u32) -> Vec<String> {
    let temp = tempfile::tempdir().unwrap();
    let first = start(temp.path());
    root(&first);
    let db = Connection::open(first.config.metadata_path()).unwrap();
    let (c, child) = prepare(&first, &db, a_pending).await;
    crash_land(&first, &db, &c, &child, crash).await;
    let at_crash = child_base(&db, &child);
    drop(first);
    let restarted = start(temp.path());
    restarted.landing.recover(&restarted).unwrap();
    let after_recovery = child_base(&db, &child);
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
        at_crash,
        after_recovery,
        retry.into(),
        landed,
        pending,
        child_base(&db, &child),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_at_every_land_step_matches_the_recovery_model() {
    let table = include_str!("../../../../proofs/fixtures/crash-recovery.tsv");
    for row in table.lines().skip(1).filter(|line| !line.trim().is_empty()) {
        let cells: Vec<&str> = row.trim_end().split('\t').collect();
        let actual = scenario(cells[0] == "1", cells[1].parse().unwrap()).await;
        assert_eq!(actual, cells[2..], "fixture row {row}");
    }
}
