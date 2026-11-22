//! End-to-end check of the state feed (issues #403 and #406,
//! spec/R9-events.md "State feed") through the real background task and `curl`, against a
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
use rosen_protocol::channel::{
    AppendEntry, Author, ChannelRequest, Intent, Ledger as ChannelLedger, SendMessage,
};
use rosen_protocol::*;
use rosen_server::{channel, landing::Ledger, rpc::dispatch, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A secret a channel message carries; it must never reach the endpoint
/// unless the project sends channel lines.
const MESSAGE: &str = "channel-message-never-sent";
/// A ledger entry's text; the state feed never sends ledger entries.
const LEDGER_TEXT: &str = "ledger-entry-not-a-channel-line";

/// What the fake endpoint holds.
#[derive(Default)]
struct Intake {
    /// The newest copy of each item by (section, key).
    held: BTreeMap<(String, String), Value>,
    through: u64,
    bodies: Vec<Vec<u8>>,
    paths: Vec<String>,
    failing: Option<u16>,
    /// When set, any batch whose body contains `needle` is refused with `status`.
    refuse: Option<(String, u16)>,
    /// `(old_path, new_path, status)` redirect rule (#446).
    redirect: Option<(String, String, u16)>,
    /// When set, batches with a different `feed_id` receive HTTP 409 `source_changed`.
    expected_feed_id: Option<String>,
    resets: usize,
    refusals: usize,
    probes: usize,
}

impl Intake {
    fn answer(&mut self, path: &str, body: &[u8]) -> (u16, Option<String>, String) {
        if let Some((old, new, code)) = &self.redirect {
            if path == old {
                self.paths.push(path.to_owned());
                return (*code, Some(new.clone()), String::new());
            }
        }
        if let Some(status) = self.failing {
            return (status, None, "{}".into());
        }
        if path.ends_with("/rosen/state/reset") {
            let parsed: Value = serde_json::from_slice(body).unwrap();
            self.held.clear();
            self.through = 0;
            self.expected_feed_id = parsed["feed_id"].as_str().map(str::to_owned);
            self.resets += 1;
            return (200, None, json!({"ok": true}).to_string());
        }
        if let Some((needle, status)) = &self.refuse {
            if String::from_utf8_lossy(body).contains(needle) {
                self.refusals += 1;
                return (*status, None, json!({"error": "invalid item"}).to_string());
            }
        }
        let parsed: Value = serde_json::from_slice(body).unwrap();
        if let Some(expected) = &self.expected_feed_id {
            if parsed["feed_id"].as_str() != Some(expected.as_str()) {
                return (409, None, json!({"error": "source_changed"}).to_string());
            }
        }
        let is_probe = parsed["through"].as_u64() == Some(0)
            && SECTIONS
                .iter()
                .all(|s| parsed[*s].as_array().is_some_and(Vec::is_empty))
            && parsed.get("relay").is_none()
            && parsed.get("platforms").is_none()
            && parsed.get("stats").is_none();
        if is_probe {
            self.probes += 1;
        }
        self.paths.push(path.to_owned());
        self.bodies.push(body.to_vec());
        self.store(body);
        (
            200,
            None,
            json!({ "highest_sequence": self.through }).to_string(),
        )
    }

    /// Keeps an item unless a newer copy is held, as Collapse does.
    fn store(&mut self, body: &[u8]) {
        let batch: Value = serde_json::from_slice(body).unwrap();
        for section in SECTIONS {
            for item in batch[section].as_array().unwrap() {
                // Lines are keyed by their own sequence, and the machine
                // list and presence are one document each.
                let key = match section {
                    "events" | "channel" => item["seq"].to_string(),
                    "machines" | "presence" | "config" => section.to_owned(),
                    _ => item
                        .get("id")
                        .or_else(|| item.get("change"))
                        .map_or("config".into(), Value::to_string),
                };
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

    /// Every held item of `section`.
    fn items(&self, section: &str) -> Vec<Value> {
        let held = self.held.iter();
        let items = held.filter(|((kind, _), _)| kind == section);
        items.map(|(_, item)| item.clone()).collect()
    }

    fn change(&self, id: &str) -> Option<Value> {
        self.held
            .get(&("changes".into(), json!(id).to_string()))
            .cloned()
    }
}

const SECTIONS: [&str; 9] = [
    "changes",
    "conflicts",
    "threads",
    "coverage",
    "config",
    "events",
    "machines",
    "presence",
    "channel",
];

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
        let (status, location, body) = intake.lock().unwrap().answer(&path, &request[head_end..]);
        let loc_header = location
            .map(|loc| format!("Location: {loc}\r\n"))
            .unwrap_or_default();
        let response = format!(
            "HTTP/1.1 {status} X\r\n{loc_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    }
}

/// A server whose base names `collapse` as the fake endpoint's root, with
/// `extra` added to its `rosen.toml`.
fn setup(path: &std::path::Path, collapse: &str, extra: &str) -> Arc<ServerState> {
    let state = ServerState::new(ServerConfig {
        data_dir: path.into(),
        endpoint: Endpoint::new("state-push-contract"),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let config = format!("review = \"after-land\"\ncollapse = \"{collapse}\"\n{extra}");
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
    rosen_server::mirror::owner::mark(path).unwrap();
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
    let state = setup(data.path(), &root, "");
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
    // The first pass also sends presence and the (empty) machine list.
    let presence = intake.lock().unwrap().items("presence");
    assert_eq!(presence[0]["members"][0]["nick"], "alice", "{presence:?}");
    assert!(intake.lock().unwrap().items("machines")[0]["machines"].is_array());

    // A post-land failure goes as an event line within 10 s, its output
    // cut to the last 64 KiB.
    let output = format!("{}tail-of-output", "x".repeat(70 * 1024));
    let detail = json!({ "kind": "test", "output": output, "requires_revert": true });
    let seq = state
        .events
        .publish(EventKind::PostLandFailure, Some(first.clone()), detail);
    let line = |i: &Intake| i.items("events").into_iter().find(|e| e["seq"] == seq);
    until(&intake, Duration::from_secs(15), |i| line(i).is_some()).await;
    let line = line(&intake.lock().unwrap()).unwrap();
    assert_eq!(line["kind"], "post_land_failure");
    assert_eq!(line["detail"]["truncated"], true);
    let kept = line["detail"]["output"].as_str().unwrap();
    assert!(kept.len() == 64 * 1024 && kept.ends_with("tail-of-output"));
    assert!(line["time"].as_i64().unwrap() > 0);

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
    assert!(
        intake.items("channel").is_empty(),
        "channel lines were sent"
    );
    feed.abort();
}

/// Risk: with `collapse_channel = true` a line sent twice or out of order
/// shows a message twice, and a ledger entry sent here duplicates the
/// ledger push.
#[tokio::test(flavor = "multi_thread")]
async fn channel_lines_go_once_in_order_without_ledger_entries() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let intake = Arc::new(Mutex::new(Intake::default()));
    tokio::spawn(serve(listener, Arc::clone(&intake)));
    let state = setup(data.path(), &root, "collapse_channel = true\n");
    let feed = tokio::spawn(rosen_server::state_push::run(Arc::clone(&state)));
    let wait = Duration::from_secs(30);
    // An endpoint never reached starts at the log's tip, so the lines
    // under test come after the first pass.
    until(&intake, wait, |i| !i.items("config").is_empty()).await;
    let join = ChannelRequest::Join {
        member: "a".into(),
        nick: "alice".into(),
        machine: "m1".into(),
    };
    channel::run(&state.events, join).unwrap();
    let entry = AppendEntry {
        ledger: ChannelLedger::Discovery,
        author: Author::Agent { member: "a".into() },
        text: LEDGER_TEXT.into(),
        velocity_issue: None,
    };
    channel::run(&state.events, ChannelRequest::Append(entry)).unwrap();
    let message = SendMessage {
        from: Author::Agent { member: "a".into() },
        to: vec!["alice".into()],
        intent: Intent::Inform,
        text: MESSAGE.into(),
    };
    channel::run(&state.events, ChannelRequest::Send(message)).unwrap();
    let sent = |i: &Intake| {
        i.bodies
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains(MESSAGE))
    };
    until(&intake, wait, sent).await;

    let intake = intake.lock().unwrap();
    let mut seqs = Vec::new();
    for body in &intake.bodies {
        let batch: Value = serde_json::from_slice(body).unwrap();
        let lines = batch["channel"].as_array().unwrap();
        seqs.extend(lines.iter().map(|line| line["seq"].as_u64().unwrap()));
        assert!(
            !String::from_utf8_lossy(body).contains(LEDGER_TEXT),
            "a ledger entry was sent"
        );
    }
    let mut ordered = seqs.clone();
    ordered.sort_unstable();
    ordered.dedup();
    assert_eq!(seqs, ordered, "channel lines repeated or out of order");
    let mut lines = intake.items("channel");
    lines.sort_by_key(|line| line["seq"].as_u64());
    let kinds: Vec<Value> = lines.iter().map(|l| l["detail"]["line"].clone()).collect();
    assert_eq!(kinds, [json!("joined"), json!("message")]);
    feed.abort();
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}

async fn serve_spy(listener: tokio::net::TcpListener, paths: Arc<Mutex<Vec<String>>>) {
    while let Ok((mut socket, _)) = listener.accept().await {
        let mut buf = [0u8; 4096];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let path = String::from_utf8_lossy(&buf[..n])
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_owned();
        paths.lock().unwrap().push(path);
        let body = r#"{"highest_sequence":0,"generation":1,"actions":[]}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(reply.as_bytes()).await;
    }
}

fn append_discovery(state: &ServerState, text: &str) {
    let entry = AppendEntry {
        ledger: ChannelLedger::Discovery,
        author: Author::User,
        text: text.into(),
        velocity_issue: None,
    };
    channel::run(&state.events, ChannelRequest::Append(entry)).unwrap();
}

/// Sets up a marked owner server in `orig` with `collapse`, a relay
/// allow-list, a ledger entry and an open change, then copies the whole
/// data directory into `copy` (including `mirror-owner`).
async fn copy_owner_project(
    orig: &std::path::Path,
    copy: &std::path::Path,
    root: &str,
) -> (Arc<ServerState>, String) {
    let first = {
        let state = setup(orig, root, "");
        rosen_server::relay::set(&state, vec![RelayKind::Config, RelayKind::Approve]).await;
        append_discovery(&state, "from owner");
        change_editing(&state, "a.txt").await
    };
    copy_dir(orig, copy);
    assert!(copy.join("mirror-owner").is_file());
    assert!(!rosen_server::mirror::owner::is_owner(copy));
    let copy_state = ServerState::new(ServerConfig {
        data_dir: copy.into(),
        endpoint: Endpoint::new("state-push-copy"),
        warm_pool_dir: None,
    });
    assert!(rosen_server::relay::setting(&copy_state)
        .await
        .unwrap()
        .active());
    (copy_state, first)
}

/// Risk (#433): a server started on a copied data directory inherits
/// `rosen.toml`, credentials and the relay allow-list; without the
/// mirror-owner gate it sends colliding sequences and runs relayed actions.
#[tokio::test(flavor = "multi_thread")]
async fn copied_data_dir_sends_nothing_to_collapse_and_runs_no_relay() {
    let (orig, copy) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let paths = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(serve_spy(listener, Arc::clone(&paths)));

    let (copy_state, first) = copy_owner_project(orig.path(), copy.path(), &root).await;
    let Response::Status(status) = call(&copy_state, Request::Status).await else {
        panic!()
    };
    assert_eq!(status.mirror_owner, Some(false));
    let collapse = status.collapse.expect("collapse health reported");
    assert_eq!(collapse.state_feed.state, FeedState::NotOwner);
    assert_eq!(collapse.ledger_push.state, FeedState::NotOwner);
    assert_eq!(collapse.relay.state, FeedState::NotOwner);

    let feed = tokio::spawn(rosen_server::state_push::run(Arc::clone(&copy_state)));
    let ledger = tokio::spawn(rosen_server::ledger_push::run(Arc::clone(&copy_state)));
    let relay = tokio::spawn(rosen_server::relay::run(Arc::clone(&copy_state)));

    append_discovery(&copy_state, "from copy");
    call(&copy_state, Request::Land { change: first }).await;
    copy_state.relay_wake.actions_pending();

    let mut session = rosen_server::ledger_push::Session::default();
    let not_owner = rosen_server::ledger_push::Pushed::NotOwner;
    assert_eq!(
        rosen_server::ledger_push::push_once(&copy_state, &mut session)
            .await
            .unwrap(),
        not_owner
    );
    let url = format!("{root}/ledgers");
    assert_eq!(
        rosen_server::ledger_push::push_to(&copy_state.config, &url, &mut session)
            .await
            .unwrap(),
        not_owner
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    for task in [feed, ledger, relay] {
        task.abort();
    }
    assert!(
        paths.lock().unwrap().is_empty(),
        "copy contacted Collapse: {:?}",
        paths.lock().unwrap()
    );
    let all = ChangeFilter {
        owner: None,
        state: None,
    };
    let Response::Changes(changes) = call(&copy_state, Request::ChangeList(all)).await else {
        panic!()
    };
    assert_eq!(changes.len(), 1);
}

async fn status_of(state: &Arc<ServerState>) -> ServerStatus {
    let Response::Status(status) = call(state, Request::Status).await else {
        panic!()
    };
    *status
}

async fn until_status(
    state: &Arc<ServerState>,
    limit: Duration,
    ready: impl Fn(&ServerStatus) -> bool,
) -> ServerStatus {
    let start = Instant::now();
    loop {
        let status = status_of(state).await;
        if ready(&status) {
            return status;
        }
        assert!(
            start.elapsed() < limit,
            "status never converged: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn state_push_events(state: &ServerState) -> Vec<Event> {
    state
        .events
        .page(0, None, 500)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == EventKind::StatePush)
        .collect()
}

/// Risk (#434, R9, R10): a revoked Collapse token or 410 stop must be
/// visible in `server status` with the prior success time and HTTP status,
/// and return to `current` with a `state_push` `sent` event once restored.
#[tokio::test(flavor = "multi_thread")]
async fn server_status_reports_collapse_401_recovery_and_410_stop() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let intake = Arc::new(Mutex::new(Intake::default()));
    tokio::spawn(serve(listener, Arc::clone(&intake)));
    let state = setup(data.path(), &root, "");
    let _first = change_editing(&state, "a.txt").await;
    let feed = tokio::spawn(rosen_server::state_push::run(Arc::clone(&state)));
    let wait = Duration::from_secs(30);

    let ok = until_status(&state, wait, |s| {
        s.collapse.as_ref().is_some_and(|c| {
            c.state_feed.state == FeedState::Current && c.state_feed.last_success_at.is_some()
        })
    })
    .await;
    let first_ok = ok.collapse.as_ref().unwrap().state_feed.last_success_at;
    assert!(ok.collapse.as_ref().unwrap().state_feed.cursor.is_some());

    // Revoke the token (HTTP 401) and create new work.
    intake.lock().unwrap().failing = Some(401);
    let _second = change_editing(&state, "b.txt").await;
    let failing = until_status(&state, wait, |s| {
        s.collapse
            .as_ref()
            .is_some_and(|c| c.state_feed.state == FeedState::Failing)
    })
    .await;
    let sf = &failing.collapse.as_ref().unwrap().state_feed;
    assert_eq!(sf.last_success_at, first_ok);
    assert!(sf.backlog > 0 && sf.next_retry_at.is_some(), "{sf:?}");
    assert_eq!(sf.last_error.as_ref().unwrap().http_status, Some(401));

    // Approve a new token: status returns to current and publishes `sent`.
    intake.lock().unwrap().failing = None;
    let recovered = until_status(&state, wait, |s| {
        s.collapse
            .as_ref()
            .is_some_and(|c| c.state_feed.state == FeedState::Current && c.state_feed.backlog == 0)
    })
    .await;
    let sf = &recovered.collapse.as_ref().unwrap().state_feed;
    assert!(
        sf.last_error.is_none() && sf.next_retry_at.is_none() && sf.last_success_at >= first_ok
    );
    let events = state_push_events(&state);
    assert!(events
        .iter()
        .any(|e| e.detail["status"] == "failed" && e.detail["http_status"] == 401));
    assert!(events
        .iter()
        .any(|e| e.detail["status"] == "sent" && e.detail["after_failures"].as_u64() >= Some(1)));

    // HTTP 410 stops sending.
    intake.lock().unwrap().failing = Some(410);
    let _third = change_editing(&state, "c.txt").await;
    let stopped = until_status(&state, wait, |s| {
        s.collapse
            .as_ref()
            .is_some_and(|c| c.state_feed.state == FeedState::Stopped)
    })
    .await;
    let sf = &stopped.collapse.as_ref().unwrap().state_feed;
    assert_eq!(sf.last_error.as_ref().unwrap().http_status, Some(410));
    assert!(sf.next_retry_at.is_none());
    feed.abort();
}

/// Risk (#444, R9, R10): one invalid item in a batch must not stall delivery
/// of valid items forever, status and events must name the refused document,
/// the next pass must not resend it unchanged, and empty probes must never
/// bump `last_success_at`.
#[tokio::test(flavor = "multi_thread")]
async fn refused_item_splits_batch_skips_unchanged_and_probes_do_not_hide_failure() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let intake = Arc::new(Mutex::new(Intake::default()));
    tokio::spawn(serve(listener, Arc::clone(&intake)));
    let state = setup(data.path(), &root, "");

    let good = change_editing(&state, "good.txt").await;
    let bad = change_editing(&state, "bad.txt").await;
    let bad_doc = format!("change:{bad}");
    intake.lock().unwrap().refuse = Some((bad.clone(), 400));

    let feed = tokio::spawn(rosen_server::state_push::run(Arc::clone(&state)));
    let wait = Duration::from_secs(30);

    until(&intake, wait, |i| i.change(&good).is_some()).await;
    assert!(intake.lock().unwrap().change(&bad).is_none());

    let status = until_status(&state, wait, |s| {
        s.collapse.as_ref().is_some_and(|c| {
            c.state_feed.state == FeedState::Current
                && c.state_feed.refused.iter().any(|r| r.document == bad_doc)
        })
    })
    .await;
    assert!(status
        .collapse
        .as_ref()
        .unwrap()
        .state_feed
        .last_success_at
        .is_some());
    assert!(state_push_events(&state).iter().any(|e| {
        e.detail["status"] == "refused"
            && e.detail["document"] == bad_doc
            && e.detail["http_status"] == 400
    }));

    // Next pass delivers a new valid change without resending `bad` unchanged.
    let refusals_before = intake.lock().unwrap().refusals;
    let later = change_editing(&state, "later.txt").await;
    until(&intake, wait, |i| i.change(&later).is_some()).await;
    assert_eq!(intake.lock().unwrap().refusals, refusals_before);

    // Once Collapse accepts `bad` and `bad` changes, it delivers and clears refusal.
    assert_refusal_clears_and_probes_keep_last_success(&state, &intake, &bad, wait).await;
    feed.abort();
}

async fn assert_refusal_clears_and_probes_keep_last_success(
    state: &Arc<ServerState>,
    intake: &Mutex<Intake>,
    bad: &str,
    wait: Duration,
) {
    intake.lock().unwrap().refuse = None;
    // Claiming a new path changes `bad`'s fingerprint and fires an urgent
    // `claim` event (`SETTLE = 2s`), unlike a bare snapshot (`SNAPSHOT_WAIT = 60s`).
    call(
        state,
        Request::ClaimPaths(ClaimRequest {
            change: bad.to_owned(),
            paths: vec!["fixed.txt".into()],
            exclusive: false,
        }),
    )
    .await;
    until(intake, wait, |i| i.change(bad).is_some()).await;
    let cleared = until_status(state, wait, |s| {
        s.collapse
            .as_ref()
            .is_some_and(|c| c.state_feed.refused.is_empty())
    })
    .await;
    let ok_before = cleared
        .collapse
        .as_ref()
        .unwrap()
        .state_feed
        .last_success_at;

    // Probes alone never bump `last_success_at` while non-probe sends fail (500).
    let probes_before = intake.lock().unwrap().probes;
    intake.lock().unwrap().refuse = Some(("\"changes\":[{".into(), 500));
    let _blocked = change_editing(state, "blocked.txt").await;
    let failing = until_status(state, wait, |s| {
        s.collapse.as_ref().is_some_and(|c| {
            c.state_feed.state == FeedState::Failing
                && intake.lock().unwrap().probes > probes_before
        })
    })
    .await;
    assert_eq!(
        failing
            .collapse
            .as_ref()
            .unwrap()
            .state_feed
            .last_success_at,
        ok_before
    );
}

/// Risk (#445, R9): a fresh data directory linked to a Collapse repo with
/// existing feed state must report `diverged` and send nothing (both on 409
/// `source_changed` and under the stopgap when Collapse holds a higher
/// sequence), then deliver its live state and lines after `CollapseReset`.
#[tokio::test(flavor = "multi_thread")]
async fn fresh_data_dir_reports_diverged_until_reset_then_delivers() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let intake = Arc::new(Mutex::new(Intake {
        through: 500,
        expected_feed_id: Some("old-server-feed-id".into()),
        ..Intake::default()
    }));
    tokio::spawn(serve(listener, Arc::clone(&intake)));
    let state = setup(data.path(), &root, "");
    let change_id = change_editing(&state, "fresh.txt").await;
    let seq = state.events.publish(
        EventKind::Green,
        Some(change_id.clone()),
        json!({ "commit": "1111111111111111111111111111111111111111" }),
    );
    let feed = tokio::spawn(rosen_server::state_push::run(Arc::clone(&state)));
    let wait = Duration::from_secs(30);

    let diverged = until_status(&state, wait, |s| {
        s.collapse
            .as_ref()
            .is_some_and(|c| c.state_feed.state == FeedState::Diverged)
    })
    .await;
    let sf = &diverged.collapse.as_ref().unwrap().state_feed;
    assert_eq!(sf.last_error.as_ref().unwrap().http_status, Some(409));
    assert!(intake.lock().unwrap().bodies.is_empty());

    let Response::CollapseReset(reset) = call(&state, Request::CollapseReset).await else {
        panic!("expected CollapseReset");
    };
    assert_eq!(intake.lock().unwrap().resets, 1);
    assert_eq!(
        intake.lock().unwrap().expected_feed_id.as_deref(),
        Some(reset.feed_id.as_str())
    );

    until(&intake, wait, |i| {
        i.change(&change_id).is_some() && i.items("events").iter().any(|e| e["seq"] == seq)
    })
    .await;
    let current = until_status(&state, wait, |s| {
        s.collapse
            .as_ref()
            .is_some_and(|c| c.state_feed.state == FeedState::Current)
    })
    .await;
    assert!(current
        .collapse
        .as_ref()
        .unwrap()
        .state_feed
        .last_error
        .is_none());
    feed.abort();
}

/// Risk (#446, R9, R10): after a Collapse rename, the state feed must follow a
/// same-origin 307 for the session so updates keep delivering while `server status`
/// reports `moved` with the new path and `rosen collapse link <new> --land` hint.
#[tokio::test(flavor = "multi_thread")]
async fn state_feed_follows_same_origin_307_and_reports_moved_status() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/o/r", listener.local_addr().unwrap());
    let intake = Arc::new(Mutex::new(Intake {
        redirect: Some((
            "/o/r/rosen/state".into(),
            "/new-org/new-repo/rosen/state".into(),
            307,
        )),
        ..Intake::default()
    }));
    tokio::spawn(serve(listener, Arc::clone(&intake)));
    let state = setup(data.path(), &root, "");
    let first = change_editing(&state, "a.txt").await;
    let feed = tokio::spawn(rosen_server::state_push::run(Arc::clone(&state)));
    let wait = Duration::from_secs(30);

    until(&intake, wait, |i| i.change(&first).is_some()).await;
    let status = until_status(&state, wait, |s| {
        s.collapse.as_ref().is_some_and(|c| {
            c.state_feed.state == FeedState::Moved && c.state_feed.last_success_at.is_some()
        })
    })
    .await;
    let sf = &status.collapse.as_ref().unwrap().state_feed;
    assert!(
        sf.moved_to
            .as_deref()
            .unwrap()
            .ends_with("/new-org/new-repo/rosen/state"),
        "{sf:?}"
    );
    assert_eq!(
        sf.hint.as_deref(),
        Some("rosen collapse link new-org/new-repo --land")
    );
    assert!(sf.last_error.is_none());
    let old_hits = intake
        .lock()
        .unwrap()
        .paths
        .iter()
        .filter(|p| *p == "/o/r/rosen/state")
        .count();
    assert_eq!(old_hits, 1);

    // Subsequent change delivers directly to the redirected path without re-hitting the 307.
    let second = change_editing(&state, "b.txt").await;
    until(&intake, wait, |i| i.change(&second).is_some()).await;
    let old_hits_after = intake
        .lock()
        .unwrap()
        .paths
        .iter()
        .filter(|p| *p == "/o/r/rosen/state")
        .count();
    assert_eq!(old_hits_after, 1);
    feed.abort();
}
