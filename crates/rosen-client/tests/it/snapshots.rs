//! SPEC.md R2: a watched write reaches the server, and a path the workspace's
//! .gitignore ignores is never uploaded. The two-second budget is measured
//! by `rosen-stress`, separately from this correctness check (#371).
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
use std::time::Duration;

struct FixedBase(BaseTip);

impl BaseSource for FixedBase {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(self.0)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_is_stored_and_ignored_files_never_are() {
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
    let (stop, watcher) = start_watcher(&endpoint, &workspace).await;

    std::fs::create_dir(workspace.join("build")).unwrap();
    std::fs::write(workspace.join("build/out.bin"), b"build output").unwrap();
    std::fs::write(workspace.join("main.rs"), b"fn main() { edited() }\n").unwrap();
    // Only a hang guard: loaded post-land runs must not enforce latency (#371).
    let event = tokio::time::timeout(Duration::from_secs(60), events.recv())
        .await
        .expect("watcher did not store the write within 60 s")
        .unwrap();
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

async fn start_watcher(
    endpoint: &Endpoint,
    workspace: &Path,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), rosen_client::Error>>,
) {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (snapshotted, mut snapshots) = tokio::sync::mpsc::unbounded_channel();
    let (endpoint, workspace) = (endpoint.clone(), workspace.to_path_buf());
    let watcher = tokio::spawn(async move {
        let stopped = async {
            let _ = stopped.await;
        };
        let report = |event| {
            assert!(!matches!(event, WatchEvent::Failed(_)), "{event:?}");
            let _ = snapshotted.send(());
        };
        watch(&endpoint, &workspace, stopped, report).await
    });
    // Startup can outlast a fixed pause on a loaded machine (#330).
    tokio::time::timeout(Duration::from_secs(60), snapshots.recv())
        .await
        .expect("the watcher's first snapshot within 60 s")
        .unwrap();
    (stop, watcher)
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
        Endpoint::new(data.join("sock").join("s.sock").to_string_lossy())
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
