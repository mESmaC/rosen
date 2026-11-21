//! End-to-end check of the relayed `config_change` action (#405, spec/R14
//! "Collapse action relay") through the real relay task, against a local
//! queue that serves actions and keeps their results, as Collapse does.
//!
//! Risk: a web form that could rewrite `rosen.toml` outside a change, or
//! open a change that lowers its own gates, would hand policy to anyone
//! holding the owner's browser session.

use std::collections::BTreeMap;
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
}

impl Queue {
    fn answer(&mut self, path: &str, body: &[u8]) -> String {
        if path.ends_with("/rosen/doorbell") {
            return json!({ "generation": self.actions.len() }).to_string();
        }
        if let Some(after) = path.split("after=").nth(1) {
            let after: i64 = after.parse().unwrap();
            // Action 1 is handed out again however far the server read,
            // as a repeat Collapse sends before it holds a result.
            let due = self.actions.iter().filter(|a| {
                let id = a["id"].as_i64().unwrap();
                id > after || id == 1
            });
            return json!({ "actions": due.cloned().collect::<Vec<_>>() }).to_string();
        }
        let id = path.split('/').rev().nth(1).unwrap().parse().unwrap();
        self.results
            .insert(id, serde_json::from_slice(body).unwrap());
        "{}".into()
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
        let body = queue.lock().unwrap().answer(&path, &request[head_end..]);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    }
}

/// A before-land project whose base holds `config` and one other file.
fn setup(path: &std::path::Path, config: &str) -> (Arc<ServerState>, ObjectId, ObjectId) {
    let state = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("relay-config-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let blob = store.put_blob(config.as_bytes()).unwrap();
    let other = store.put_blob(b"kept\n").unwrap();
    let entries = vec![
        TreeEntry::new(FileMode::Regular, "rosen.toml", blob).unwrap(),
        TreeEntry::new(FileMode::Regular, "a.txt", other).unwrap(),
    ];
    let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
    let who = Signature::new("test", "test@test", 0);
    let commit = Commit {
        tree,
        parents: vec![],
        author: who.clone(),
        committer: who,
        extra_headers: vec![],
        message: b"base\n".to_vec(),
    };
    let base = store.put_commit(&commit).unwrap();
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .init_base("main", &base)
        .unwrap();
    rosen_server::mirror::owner::mark(path).unwrap();
    (state, base, blob)
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
