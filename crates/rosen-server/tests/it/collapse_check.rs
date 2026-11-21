//! End-to-end check of `rosen collapse check` (`Request::CollapseCheck`, SPEC R1, R10, #435).
//!
//! Risk: a misconfigured mirror owner, mismatched `collapse` vs `[mirror]`
//! repository, or failing state/ledger/doorbell endpoint silently breaks the
//! Collapse link; `collapse check` must detect each failure without mutating
//! server state.

use std::process::Command;
use std::sync::{Arc, Mutex};

use rosen_core::{Commit, FileMode, Signature, Tree, TreeEntry};
use rosen_protocol::{
    CollapseCheckItem, CollapseCheckReport, CollapseCheckStatus, Endpoint, Outcome, RelayKind,
    Request, Response,
};
use rosen_server::landing::Ledger;
use rosen_server::mirror::{owner, MirrorStore};
use rosen_server::rpc::dispatch;
use rosen_server::{ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct MockCollapse {
    state_status: Option<u16>,
    state_body: Option<String>,
    state_highest: u64,
    ledger_highest: (u64, u64),
    requests: Vec<(String, String)>,
}

impl MockCollapse {
    fn answer(&mut self, method: &str, path: &str, body: &[u8]) -> (u16, String) {
        self.requests.push((method.to_owned(), path.to_owned()));
        if path.ends_with("/rosen/state") {
            if let Some(status) = self.state_status {
                let body = self
                    .state_body
                    .clone()
                    .unwrap_or_else(|| json!({"error": "unauthorized"}).to_string());
                return (status, body);
            }
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert_eq!(parsed["through"], 0);
            assert!(parsed["feed_id"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(parsed["changes"].as_array().unwrap().is_empty());
            return (
                200,
                json!({"highest_sequence": self.state_highest}).to_string(),
            );
        }
        if path.ends_with("/ledgers") {
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert!(parsed["feed_id"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(parsed["entries"].as_array().unwrap().is_empty());
            return (
                200,
                json!({"highest_sequence": {
                    "discovery": self.ledger_highest.0,
                    "past_work": self.ledger_highest.1
                }})
                .to_string(),
            );
        }
        if path.ends_with("/rosen/doorbell") {
            return (200, json!({"generation": 5}).to_string());
        }
        (404, json!({"error": "not found"}).to_string())
    }
}

async fn serve(listener: tokio::net::TcpListener, mock: Arc<Mutex<MockCollapse>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buffer = [0u8; 8192];
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
        let first_line = String::from_utf8_lossy(&request[..head_end]);
        let mut parts = first_line.lines().next().unwrap_or("").split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let (status, body) = mock
            .lock()
            .unwrap()
            .answer(method, path, &request[head_end..]);
        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    }
}

fn init_base_config(state: &ServerState, toml_text: &str) {
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let blob = store.put_blob(toml_text.as_bytes()).unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "rosen.toml", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
    let sig = Signature::new("test", "test@example.com", 1_700_000_000);
    let commit = store
        .put_commit(&Commit {
            tree,
            parents: vec![],
            author: sig.clone(),
            committer: sig,
            extra_headers: vec![],
            message: b"base\n".to_vec(),
        })
        .unwrap();
    let ledger = Ledger::open(&state.config.metadata_path()).unwrap();
    ledger.init_base("main", &commit).unwrap();
}

fn check(runtime: &tokio::runtime::Runtime, state: &Arc<ServerState>) -> CollapseCheckReport {
    match runtime.block_on(dispatch(state, Request::CollapseCheck)) {
        Outcome::Ok(Response::CollapseCheck(report)) => *report,
        other => panic!("unexpected response: {other:?}"),
    }
}

fn find<'a>(report: &'a CollapseCheckReport, name: &str) -> &'a CollapseCheckItem {
    report
        .checks
        .iter()
        .find(|item| item.name == name)
        .unwrap_or_else(|| panic!("missing check {name} in {:?}", report.checks))
}

fn setup_linked_server(
    runtime: &tokio::runtime::Runtime,
    addr: std::net::SocketAddr,
    data: &std::path::Path,
    bare: &std::path::Path,
) -> Arc<ServerState> {
    assert!(Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(bare)
        .status()
        .unwrap()
        .success());
    owner::mark(data).unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.to_path_buf(),
        endpoint: Endpoint::new("rosen-collapse-check-test-unused"),
        warm_pool_dir: None,
    });
    let collapse_root = format!("http://{addr}/mESmaC/rosen");
    init_base_config(&state, &format!("collapse = {collapse_root:?}\n"));
    MirrorStore::open(&state.config.metadata_path())
        .unwrap()
        .set_remote(&bare.display().to_string())
        .unwrap();
    let _ = runtime.block_on(dispatch(
        &state,
        Request::RelaySet {
            allow: vec![RelayKind::Approve, RelayKind::Land],
        },
    ));
    state
}

#[test]
fn collapse_check_passes_when_linked_and_pinpoints_each_failure() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let mock = Arc::new(Mutex::new(MockCollapse::default()));
    let _server = runtime.spawn(serve(listener, Arc::clone(&mock)));
    let data = tempfile::tempdir().unwrap();
    let bare = tempfile::tempdir().unwrap();
    let state = setup_linked_server(&runtime, addr, data.path(), bare.path());

    // 1. Correctly linked server passes every active check.
    let report = check(&runtime, &state);
    assert!(report.ok, "{report:?}");
    for name in [
        "mirror_owner",
        "collapse_config",
        "mirror_ls_remote",
        "state_endpoint",
        "ledger_endpoint",
        "relay_allow",
        "relay_doorbell",
    ] {
        assert_eq!(
            find(&report, name).status,
            CollapseCheckStatus::Pass,
            "{name}"
        );
    }

    // 2. Removing the mirror-owner mark fails only `mirror_owner` with a hint.
    owner::unmark(data.path()).unwrap();
    let report = check(&runtime, &state);
    assert!(!report.ok);
    let own = find(&report, "mirror_owner");
    assert_eq!(own.status, CollapseCheckStatus::Fail);
    assert!(own.hint.as_deref().unwrap().contains("--mirror-owner"));
    owner::mark(data.path()).unwrap();

    // 3. State endpoint rejecting auth fails only `state_endpoint` with a hint.
    mock.lock().unwrap().state_status = Some(401);
    let report = check(&runtime, &state);
    assert!(!report.ok);
    assert_eq!(
        find(&report, "state_endpoint").status,
        CollapseCheckStatus::Fail
    );
    assert!(find(&report, "state_endpoint")
        .hint
        .as_deref()
        .unwrap()
        .contains("git credential approve"));
    assert_eq!(
        find(&report, "ledger_endpoint").status,
        CollapseCheckStatus::Pass
    );
    assert_eq!(
        find(&report, "relay_doorbell").status,
        CollapseCheckStatus::Pass
    );
    mock.lock().unwrap().state_status = None;

    // 3b. HTTP 409 source_changed and stopgap (Collapse ahead of local log) report `diverged`.
    {
        let mut m = mock.lock().unwrap();
        m.state_status = Some(409);
        m.state_body = Some(json!({"error": "source_changed"}).to_string());
        m.ledger_highest = (7, 3);
    }
    let report = check(&runtime, &state);
    assert!(!report.ok);
    let sf = find(&report, "state_endpoint");
    assert_eq!(sf.status, CollapseCheckStatus::Fail);
    assert!(sf.detail.contains("diverged"), "{sf:?}");
    assert!(sf.hint.as_deref().unwrap().contains("--reset"), "{sf:?}");
    let lf = find(&report, "ledger_endpoint");
    assert_eq!(lf.status, CollapseCheckStatus::Fail);
    assert!(lf.detail.contains("diverged"), "{lf:?}");
    assert!(lf.hint.as_deref().unwrap().contains("--reset"), "{lf:?}");
    {
        let mut m = mock.lock().unwrap();
        m.state_status = None;
        m.state_body = None;
        m.state_highest = 42;
        m.ledger_highest = (0, 0);
    }
    let report = check(&runtime, &state);
    assert!(!report.ok);
    assert!(
        find(&report, "state_endpoint").detail.contains("diverged"),
        "{report:?}"
    );
    mock.lock().unwrap().state_highest = 0;

    // 4. Pointing `collapse` at a different repository fails `repo_match` with a hint.
    let mismatch_dir = tempfile::tempdir().unwrap();
    owner::mark(mismatch_dir.path()).unwrap();
    let mismatch_state = ServerState::new(ServerConfig {
        data_dir: mismatch_dir.path().to_path_buf(),
        endpoint: Endpoint::new("rosen-collapse-check-mismatch-unused"),
        warm_pool_dir: None,
    });
    let mismatch = format!(
        "collapse = \"http://{addr}/mESmaC/other\"\n[mirror]\nremote = \"https://collapse.invalid/mESmaC/rosen.git\"\n"
    );
    init_base_config(&mismatch_state, &mismatch);
    let report = check(&runtime, &mismatch_state);
    assert!(!report.ok);
    let matched = find(&report, "repo_match");
    assert_eq!(matched.status, CollapseCheckStatus::Fail, "{matched:?}");
    assert!(matched.detail.contains("mESmaC/other"));
    assert!(matched.detail.contains("mESmaC/rosen"));
    assert!(matched.hint.as_deref().unwrap().contains("rosen.toml"));
}
