//! End-to-end check of the state feed (issue #403, spec/R9-events.md
//! "State feed") through the real background task and `curl`, against a
//! local endpoint that keeps the newer copy of each item, as Collapse does.
//!
//! Risk: the feed is Collapse's only view of unlanded work. One that stops
//! after an outage leaves the Review tab silently stale, and one that sends
//! channel messages, receipt signatures or credentials leaks data the
//! endpoint must never hold.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rosen_core::{AgentId, Commit, FileMode, Signature, Tree, TreeEntry};
use rosen_protocol::channel::{Author, ChannelRequest, Intent, SendMessage};
use rosen_protocol::*;
use rosen_server::{channel, landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A secret a channel message carries; it must never reach the endpoint.
const MESSAGE: &str = "channel-message-never-sent";

/// What the fake endpoint holds.
#[derive(Default)]
struct Intake {
    /// The newest copy of each item by (section, key).
    held: BTreeMap<(String, String), Value>,
    through: u64,
    bodies: Vec<Vec<u8>>,
    paths: Vec<String>,
    failing: Option<u16>,
}

impl Intake {
    fn answer(&mut self, path: &str, body: &[u8]) -> (u16, String) {
        if let Some(status) = self.failing {
            return (status, "{}".into());
        }
        self.paths.push(path.to_owned());
        self.bodies.push(body.to_vec());
        self.store(body);
        (200, json!({ "highest_sequence": self.through }).to_string())
    }

    /// Keeps an item unless a newer copy is held, as Collapse does.
    fn store(&mut self, body: &[u8]) {
        let batch: Value = serde_json::from_slice(body).unwrap();
        for section in ["changes", "conflicts", "threads", "coverage", "config"] {
            for item in batch[section].as_array().unwrap() {
                let key = item
                    .get("id")
                    .or_else(|| item.get("change"))
                    .map_or("config".into(), Value::to_string);
                let slot = self
                    .held
                    .entry((section.into(), key))
                    .or_insert(Value::Null);
                if slot["seq"].as_u64() <= item["seq"].as_u64() {
                    *slot = item.clone();
                }
            }
        }
        self.through = self.through.max(batch["through"].as_u64().unwrap());
    }

    fn change(&self, id: &str) -> Option<Value> {
        self.held
            .get(&("changes".into(), json!(id).to_string()))
            .cloned()
    }
}

async fn serve(listener: tokio::net::TcpListener, intake: Arc<Mutex<Intake>>) {
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
        let (status, body) = intake.lock().unwrap().answer(&path, &request[head_end..]);
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    }
}

/// A server whose base names `collapse` as the fake endpoint's root.
fn setup(path: &std::path::Path, collapse: &str) -> Arc<ServerState> {
    let state = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("state-push-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let config = format!("review = \"after-land\"\ncollapse = \"{collapse}\"\n");
    let blob = store.put_blob(config.as_bytes()).unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "rosen.toml", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
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
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    ledger.init_base("main", &root).unwrap();
    state
}

async fn call(state: &Arc<ServerState>, request: Request) -> Response {
    match dispatch(state, request).await {
        Outcome::Ok(response) => response,
        other => panic!("{other:?}"),
    }
}

/// A change that wrote `path`, claimed it and holds one review thread.
async fn change_editing(state: &Arc<ServerState>, path: &str) -> String {
    let owner = AgentId::new("owner").unwrap();
    let new = NewChange {
        owner: owner.clone(),
        on: None,
    };
    let Response::Change(record) = call(state, Request::ChangeNew(new)).await else {
        panic!()
    };
    let id = record.change.id.to_string();
    let write = WorkspaceWrite {
        change: id.clone(),
        path: path.into(),
        data: Some(WorkspaceBytes(b"one\ntwo\n".to_vec())),
    };
    call(state, Request::WorkspaceWrite(write)).await;
    let claim = ClaimRequest {
        change: id.clone(),
        paths: vec![path.into()],
        exclusive: false,
    };
    call(state, Request::ClaimPaths(claim)).await;
    let comment = CommentRequest {
        change: id.clone(),
        author: owner,
        body: "check line one".into(),
        head: None,
        target: CommentTarget::Line {
            path: path.into(),
            line: 1,
        },
    };
    call(state, Request::ReviewComment(comment)).await;
    id
}

/// Waits until `held` is true of the intake, up to `limit`.
async fn until(intake: &Mutex<Intake>, limit: Duration, held: impl Fn(&Intake) -> bool) {
    let start = Instant::now();
    while !held(&intake.lock().unwrap()) {
        assert!(start.elapsed() < limit, "the endpoint never converged");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn state_feed_converges_after_an_outage_and_sends_nothing_private() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let intake = Arc::new(Mutex::new(Intake::default()));
    tokio::spawn(serve(listener, Arc::clone(&intake)));
    let state = setup(data.path(), &root);
    let join = ChannelRequest::Join {
        member: "a".into(),
        nick: "alice".into(),
        machine: "m1".into(),
    };
    channel::run(&state.events, join).unwrap();
    let message = SendMessage {
        from: Author::Agent { member: "a".into() },
        to: vec!["alice".into()],
        intent: Intent::Inform,
        text: MESSAGE.into(),
    };
    channel::run(&state.events, ChannelRequest::Send(message)).unwrap();
    let first = change_editing(&state, "a.txt").await;
    let feed = tokio::spawn(rosen_server::state_push::run(Arc::clone(&state)));

    // The open change arrives whole: claims, thread, edits and land check.
    let wait = Duration::from_secs(30);
    until(&intake, wait, |i| i.change(&first).is_some()).await;
    let doc = intake.lock().unwrap().change(&first).unwrap();
    assert_eq!(doc["state"], "open");
    assert_eq!(doc["claims"][0]["kind"], "notice");
    assert_eq!(doc["edits"][0]["added"], 2);
    assert_eq!(doc["threads"].as_array().unwrap().len(), 1);
    assert!(doc["land_check"]["landable"].is_boolean(), "{doc}");
    assert!(intake
        .lock()
        .unwrap()
        .held
        .contains_key(&("config".into(), "config".into())));

    // A land is sent once as landed, with coverage.
    call(
        &state,
        Request::Land {
            change: first.clone(),
        },
    )
    .await;
    let landed = |i: &Intake| i.change(&first).is_some_and(|doc| doc["state"] == "landed");
    until(&intake, wait, landed).await;
    let coverage = ("coverage".to_owned(), json!(first).to_string());
    assert!(intake.lock().unwrap().held.contains_key(&coverage));

    // During an outage work goes on; afterwards the endpoint converges.
    intake.lock().unwrap().failing = Some(503);
    let second = change_editing(&state, "b.txt").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    intake.lock().unwrap().failing = None;
    until(&intake, wait, |i| i.change(&second).is_some()).await;

    // A repeated batch changes nothing.
    let mut intake = intake.lock().unwrap();
    let before = intake.held.clone();
    for body in intake.bodies.clone() {
        intake.store(&body);
    }
    assert_eq!(intake.held, before);
    assert!(intake.paths.iter().all(|path| path == "/o/r/rosen/state"));
    for body in &intake.bodies {
        let text = String::from_utf8_lossy(body);
        assert!(!text.contains(MESSAGE), "a channel message was sent");
        assert!(!text.contains("signature"), "a receipt signature was sent");
    }
    feed.abort();
}
