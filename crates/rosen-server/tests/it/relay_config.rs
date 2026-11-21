//! End-to-end check of the relayed `config_change` action (#405, spec/R14
//! "Collapse action relay") through the real relay task, against a local
//! queue that serves actions and keeps their results, as Collapse does.
//!
//! Risk: a web form that could rewrite `rosen.toml` outside a change, or
//! open a change that lowers its own gates, would hand policy to anyone
//! holding the owner's browser session.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rosen_core::{AgentId, Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_protocol::*;
use rosen_server::{landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The fake Collapse: queued actions, and the results posted for them.
#[derive(Default)]
struct Queue {
    actions: Vec<Value>,
    results: BTreeMap<i64, Value>,
    status_413: BTreeSet<i64>,
    rejected: BTreeMap<i64, Vec<u16>>,
    /// `(old_prefix, new_prefix)`: old doorbell returns 404 (like Collapse's
    /// edge Worker), while old actions/results return 307 to `new_prefix` (#446).
    renamed: Option<(String, String)>,
}

impl Queue {
    fn answer(&mut self, path: &str, body: &[u8]) -> (u16, Option<String>, String) {
        if let Some((old, new)) = &self.renamed {
            if let Some(rest) = path.strip_prefix(old.as_str()) {
                if rest == "/rosen/doorbell" {
                    return (404, None, r#"{"error":"not found"}"#.into());
                }
                return (307, Some(format!("{new}{rest}")), String::new());
            }
        }
        if path.ends_with("/rosen/doorbell") {
            return (
                200,
                None,
                json!({ "generation": self.actions.len() }).to_string(),
            );
        }
        if let Some(after) = path.split("after=").nth(1) {
            let after: i64 = after.parse().unwrap();
            // Action 1 is handed out again however far the server read,
            // as a repeat Collapse sends before it holds a result.
            let due: Vec<_> = self
                .actions
                .iter()
                .filter(|a| {
                    let id = a["id"].as_i64().unwrap();
                    id > after || id == 1
                })
                .cloned()
                .collect();
            return (200, None, json!({ "actions": due }).to_string());
        }
        let id = path.split('/').rev().nth(1).unwrap().parse().unwrap();
        if body.len() > 16 << 10 {
            let status = if self.status_413.contains(&id) {
                413
            } else {
                400
            };
            self.rejected.entry(id).or_default().push(status);
            return (status, None, r#"{"error":"body too large"}"#.into());
        }
        self.results
            .insert(id, serde_json::from_slice(body).unwrap());
        (200, None, "{}".into())
    }
}

async fn serve(listener: tokio::net::TcpListener, queue: Arc<Mutex<Queue>>) {
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 65536];
        let (head_end, length) = loop {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                break (end + 4, length);
            }
        };
        while request.len() < head_end + length {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
        }
        let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
        let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
        let (status, location, body) = queue.lock().unwrap().answer(&path, &request[head_end..]);
        let reason = match status {
            200 => "OK",
            307 => "Temporary Redirect",
            400 => "Bad Request",
            404 => "Not Found",
            413 => "Payload Too Large",
            _ => "Error",
        };
        let loc_header = location
            .map(|loc| format!("Location: {loc}\r\n"))
            .unwrap_or_default();
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\n{loc_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    }
}

/// A project whose base holds `config` and `files` (each with `b"kept\n"`).
fn setup_files(
    path: &std::path::Path,
    config: &str,
    files: &[String],
) -> (Arc<ServerState>, ObjectId, ObjectId) {
    let state = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("relay-config-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let blob = store.put_blob(config.as_bytes()).unwrap();
    let other = store.put_blob(b"kept\n").unwrap();
    let mut entries = vec![TreeEntry::new(FileMode::Regular, "rosen.toml", blob).unwrap()];
    entries.extend(
        files
            .iter()
            .map(|name| TreeEntry::new(FileMode::Regular, name.as_str(), other).unwrap()),
    );
    let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let base = store
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
        .init_base("main", &base)
        .unwrap();
    rosen_server::mirror::owner::mark(path).unwrap();
    (state, base, blob)
}

fn setup(path: &std::path::Path, config: &str) -> (Arc<ServerState>, ObjectId, ObjectId) {
    setup_files(path, config, &["a.txt".to_owned()])
}

fn action(id: i64, base: ObjectId, expected: Value, text: &str) -> Value {
    json!({
        "id": id,
        "kind": "config_change",
        "account": "owner",
        "expires_at": i64::MAX,
        "arguments": {
            "base": base, "expected_blob": expected, "text": text,
            "message": "rosen.toml: review = after-land",
        },
    })
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn config_change_opens_one_gated_change_and_refuses_bad_or_stale_files() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let base_text = format!("review = \"before-land\"\ncollapse = \"{root}\"\n");
    let (state, base, blob) = setup(data.path(), &base_text);
    // The edit tries to lower the very gate it must pass.
    let lowered = format!("review = \"after-land\"\ncollapse = \"{root}\"\n");
    let queue = Arc::new(Mutex::new(Queue {
        actions: vec![
            action(1, base, json!(blob), &lowered),
            action(2, base, json!(blob), "reviw = \"after-land\"\n"),
            action(3, base, Value::Null, &lowered),
        ],
        ..Queue::default()
    }));
    tokio::spawn(serve(listener, Arc::clone(&queue)));
    rosen_server::relay::set(&state, vec![RelayKind::Config]).await;
    let relay = tokio::spawn(rosen_server::relay::run(Arc::clone(&state)));
    let start = Instant::now();
    while queue.lock().unwrap().results.len() < 3 {
        assert!(start.elapsed() < Duration::from_secs(60), "no results");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    relay.abort();
    let results = queue.lock().unwrap().results.clone();
    let made = &results[&1]["result"];
    assert_eq!(results[&1]["state"], "done", "{results:?}");
    assert_eq!(made["owner"], "owner@collapse");
    let refused = |id: i64, code: &str| {
        assert_eq!(results[&id]["state"], "refused", "{results:?}");
        assert_eq!(results[&id]["result"]["code"], code, "{results:?}");
    };
    refused(2, "invalid_config");
    assert!(results[&2]["result"]["error"]
        .as_str()
        .unwrap()
        .contains("reviw"));
    refused(3, "stale_config");

    // One change, the repeat of action 1 and both refusals adding none.
    let owner = AgentId::new("owner").unwrap();
    let filter = ChangeFilter {
        owner: Some(owner),
        state: None,
    };
    let Response::Changes(changes) = call(&state, Request::ChangeList(filter)).await else {
        panic!()
    };
    assert_eq!(changes.len(), 1);
    let change = &changes[0].change;
    assert_eq!(json!(change.id), made["change"]);
    assert_eq!(change.owner_machine.as_deref(), Some("collapse"));
    let titled = change.snapshots.iter().any(|s| {
        matches!(&s.kind, rosen_core::SnapshotKind::Named(m)
            if m.trim_end() == "rosen.toml: review = after-land")
    });
    assert!(titled, "{change:?}");

    // Its head differs from the base only in rosen.toml.
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let head = store.read_tree(&change.head).unwrap();
    let new_blob = head.get(b"rosen.toml").unwrap().id;
    assert_eq!(store.read_blob(&new_blob).unwrap().data, lowered.as_bytes());
    assert_eq!(head.entries().len(), 2);
    assert!(head.get(b"a.txt").is_some());

    // The base's before-land policy still gates it.
    let land = Request::Land {
        change: change.id.to_string(),
    };
    let Response::Land(answer) = call(&state, land).await else {
        panic!()
    };
    assert!(
        matches!(&answer.outcome, LandStatus::Refused { reason, .. } if reason == "unapproved"),
        "{answer:?}"
    );
}

/// Builds a change whose `conflicts` `LandAnswer` is 16,370 bytes: under
/// 16,384 alone, over 16,384 once wrapped in `{"result","state"}`.
async fn conflicting_change_16370(
    state: &Arc<ServerState>,
    config_blob: ObjectId,
    paths: &[String],
) -> String {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let owner = AgentId::new("owner").unwrap();
    let mut ids = Vec::new();
    for data in [b"ours\n".as_slice(), b"theirs\n".as_slice()] {
        let blob = store.put_blob(data).unwrap();
        let mut entries =
            vec![TreeEntry::new(FileMode::Regular, "rosen.toml", config_blob).unwrap()];
        entries.extend(
            paths
                .iter()
                .map(|p| TreeEntry::new(FileMode::Regular, p.as_str(), blob).unwrap()),
        );
        let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
        let create = Request::ChangeNew(NewChange {
            owner: owner.clone(),
            on: None,
        });
        let Response::Change(rec) = call(state, create).await else {
            panic!()
        };
        let snap = SnapshotRequest {
            change: rec.change.id.to_string(),
            tree,
            from: Some(rec.change.head),
            message: None,
        };
        call(state, Request::Snapshot(snap)).await;
        ids.push(rec.change.id);
    }
    let land = |id: rosen_core::ChangeId| Request::Land {
        change: id.to_string(),
    };
    let Response::Land(landed) = call(state, land(ids[1])).await else {
        panic!()
    };
    assert!(matches!(landed.outcome, LandStatus::Landed { .. }));
    state.landing.settle();
    let Response::Land(raw) = call(state, land(ids[0])).await else {
        panic!()
    };
    assert_eq!(serde_json::to_vec(&raw).unwrap().len(), 16_370);
    ids[0].to_string()
}

fn seed_legacy_oversized(state: &ServerState, root: &str) {
    let conn = rusqlite::Connection::open(state.config.metadata_path()).unwrap();
    conn.execute(
        "INSERT INTO machines (id, name, role, status, last_contact, credential_hash, expires_at) \
         VALUES ('lost', 'laptop', 'agent', 'active', 0, 'x', 0)",
        [],
    )
    .unwrap();
    let big = json!({
        "schema": "rosen.error/v1", "command": "land", "code": "internal", "error": "x".repeat(20 << 10)
    })
    .to_string();
    for id in [1i64, 2i64] {
        conn.execute(
            "INSERT INTO relay_actions (endpoint, id, action, state, result, posted, started_at) \
             VALUES (?1, ?2, '{}', 'refused', ?3, 0, 0)",
            rusqlite::params![root, id, &big],
        )
        .unwrap();
    }
}

fn unposted_count(state: &ServerState) -> i64 {
    let conn = rusqlite::Connection::open(state.config.metadata_path()).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM relay_actions WHERE posted = 0",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn assert_oversized_results(q: &Queue, state: &ServerState) {
    assert_eq!(q.rejected.get(&1).map(Vec::as_slice), Some(&[400][..]));
    assert_eq!(q.rejected.get(&2).map(Vec::as_slice), Some(&[413][..]));
    for id in [1i64, 2i64] {
        assert_eq!(q.results[&id]["state"], "refused");
        assert_eq!(q.results[&id]["result"]["schema"], "rosen.error/v1");
        assert_eq!(q.results[&id]["result"]["code"], "result_too_large");
    }
    assert!(!q.rejected.contains_key(&3) && !q.rejected.contains_key(&4));
    assert_eq!(q.results[&3]["result"]["outcome"]["reason"], "conflicts");
    assert_eq!(q.results[&3]["result"]["outcome"]["truncated"], true);
    assert_eq!(q.results[&4]["result"]["code"], "not_found");
    assert!(q.results[&4]["result"]["error"]
        .as_str()
        .unwrap()
        .ends_with("..."));
    assert_eq!(q.results[&5]["state"], "done");
    assert_eq!(q.results[&5]["result"]["status"], "revoked");
    assert_eq!(unposted_count(state), 0);
}

// Risk (R14, #443): Collapse rejects result POST bodies over 16,384 bytes with
// 400, and the relay re-posts every unposted outcome before fetching new ones,
// so one oversized outcome used to block every later action forever.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_outcomes_and_400_413_never_block_later_actions() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let paths: Vec<String> = (0..200)
        .map(|i| format!("f{i:03}_{}.txt", "a".repeat(if i < 24 { 69 } else { 68 })))
        .collect();
    let config = format!("review = \"after-land\"\ncollapse = \"{root}\"\n");
    let (state, _, blob) = setup_files(data.path(), &config, &paths);
    let change_id = conflicting_change_16370(&state, blob, &paths).await;
    rosen_server::relay::set(&state, vec![RelayKind::Land, RelayKind::Revoke]).await;
    seed_legacy_oversized(&state, &root);

    let act = |id: i64, kind: &str, arguments: Value| json!({ "id": id, "kind": kind, "account": "owner", "expires_at": i64::MAX, "arguments": arguments });
    let queue = Arc::new(Mutex::new(Queue {
        actions: vec![
            act(3, "land", json!({ "change": change_id })),
            act(
                4,
                "machine_revoke",
                json!({ "machine": "m".repeat(20 << 10) }),
            ),
            act(5, "machine_revoke", json!({ "machine": "lost" })),
        ],
        status_413: BTreeSet::from([2]),
        ..Queue::default()
    }));
    tokio::spawn(serve(listener, Arc::clone(&queue)));
    let relay = tokio::spawn(rosen_server::relay::run(Arc::clone(&state)));
    let start = Instant::now();
    // Wait until the relay has both delivered all 5 results and marked every
    // row posted in SQLite before aborting it.
    while queue.lock().unwrap().results.len() < 5 || unposted_count(&state) > 0 {
        assert!(start.elapsed() < Duration::from_secs(60), "no results");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    relay.abort();
    assert_oversized_results(&queue.lock().unwrap(), &state);
}

/// Risk (#446, R14): Collapse's edge doorbell Worker returns 404 after a rename
/// while origin `/rosen/*` routes return 307; the relay must recover, keep
/// running actions, and report `moved` with the new path in `server status`.
#[tokio::test(flavor = "multi_thread")]
async fn relay_recovers_after_rename_404_doorbell_and_reports_moved() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let base_text = format!("review = \"before-land\"\ncollapse = \"{root}\"\n");
    let (state, base, blob) = setup(data.path(), &base_text);
    let lowered = format!("review = \"after-land\"\ncollapse = \"{root}\"\n");
    let queue = Arc::new(Mutex::new(Queue {
        actions: vec![action(1, base, json!(blob), &lowered)],
        renamed: Some(("/o/r".into(), "/new-org/new-repo".into())),
        ..Queue::default()
    }));
    tokio::spawn(serve(listener, Arc::clone(&queue)));
    rosen_server::relay::set(&state, vec![RelayKind::Config]).await;
    let relay = tokio::spawn(rosen_server::relay::run(Arc::clone(&state)));
    let start = Instant::now();
    while queue.lock().unwrap().results.is_empty() {
        assert!(start.elapsed() < Duration::from_secs(30), "no relay result");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let Response::Status(status) = call(&state, Request::Status).await else {
        panic!()
    };
    relay.abort();
    let rel = &status.collapse.unwrap().relay;
    assert_eq!(rel.state, FeedState::Moved, "{rel:?}");
    assert!(
        rel.moved_to
            .as_deref()
            .unwrap()
            .ends_with("/new-org/new-repo"),
        "{rel:?}"
    );
    assert_eq!(
        rel.hint.as_deref(),
        Some("rosen collapse link new-org/new-repo --land")
    );
    assert_eq!(queue.lock().unwrap().results[&1]["state"], "done");
}
