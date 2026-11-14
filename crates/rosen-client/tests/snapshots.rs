//! SPEC.md R2: a write to a watched workspace is on the server within 2 s,
//! and a path the workspace's .gitignore ignores is never uploaded.
//!
//! Risk: write-through snapshots are what make an agent's work durable
//! (D17). A watcher that misses or delays a write leaves edits only on the
//! client, where a crash or a workspace deletion loses them; one that
//! uploads build output fills the store and counts it as an edit.

use rosen_client::protocol::{EventKind, Listener, NewChange};
use rosen_client::snapshot::{watch, WatchEvent};
use rosen_client::workspace::Options;
use rosen_client::{Client, Endpoint};
use rosen_core::{AgentId, FileMode, ObjectId, Tree, TreeEntry};
use rosen_server::changes::{BaseSource, BaseTip};
use rosen_server::{ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct FixedBase(BaseTip);

impl BaseSource for FixedBase {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(self.0)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_is_stored_within_two_seconds_and_ignored_files_never_are() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let store = LooseObjectStore::open(data.join("objects")).unwrap();
    let endpoint = start_server(&data, &store);
    let client = Client::connect(&endpoint).await.unwrap();
    let owner = AgentId::new("agent").unwrap();
    let change = client
        .change_new(NewChange { owner, on: None })
        .await
        .unwrap();
    let workspace = dir.path().join("ws");
    client
        .materialise_change(&change, &workspace, Options::default())
        .await
        .unwrap();
    let mut events = client.subscribe(None).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let watcher = tokio::spawn({
        let (endpoint, workspace) = (endpoint.clone(), workspace.clone());
        async move {
            let stopped = async {
                let _ = stopped.await;
            };
            let failed = |event| assert!(!matches!(event, WatchEvent::Failed(_)), "{event:?}");
            watch(&endpoint, &workspace, stopped, failed).await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    std::fs::create_dir(workspace.join("build")).unwrap();
    std::fs::write(workspace.join("build/out.bin"), b"build output").unwrap();
    std::fs::write(workspace.join("main.rs"), b"fn main() { edited() }\n").unwrap();
    let written = Instant::now();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("no snapshot within 2 s of the write")
        .unwrap();
    assert!(written.elapsed() < Duration::from_secs(2));
    assert_eq!(event.kind, EventKind::Snapshot);

    let tree: ObjectId = event.detail["tree"].as_str().unwrap().parse().unwrap();
    let tree = store.read_tree(&tree).unwrap();
    let main = tree.get(b"main.rs").expect("main.rs is in the snapshot");
    assert_eq!(
        store.read_blob(&main.id).unwrap().data,
        b"fn main() { edited() }\n"
    );
    assert!(tree.get(b"build").is_none(), "ignored output was uploaded");
    assert!(
        tree.get(b".rosen").is_none(),
        "workspace state was uploaded"
    );
    stop.send(()).unwrap();
    watcher.await.unwrap().unwrap();
}

/// An in-process server whose base holds `main.rs` and a `.gitignore`
/// ignoring `build/`.
fn start_server(data: &Path, store: &dyn ObjectStore) -> Endpoint {
    let entries = vec![
        file(store, ".gitignore", b"build/\n"),
        file(store, "main.rs", b"fn main() {}\n"),
    ];
    let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
    let name = format!("rosen-snapshot-test-{}", std::process::id());
    let endpoint = if cfg!(windows) {
        Endpoint::new(name)
    } else {
        Endpoint::new(data.join("s.sock").to_string_lossy())
    };
    let listener = Listener::bind(&endpoint).unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.to_path_buf(),
        endpoint: endpoint.clone(),
        warm_pool_dir: None,
    });
    let commit = ObjectId::hash_object("commit", b"base");
    let base = FixedBase(BaseTip { commit, tree });
    state.changes.set_base_source(Arc::new(base));
    tokio::spawn(rosen_server::listener::serve(listener, state));
    endpoint
}

fn file(store: &dyn ObjectStore, name: &str, body: &[u8]) -> TreeEntry {
    let blob = store.put_blob(body).unwrap();
    TreeEntry::new(FileMode::Regular, name, blob).unwrap()
}
