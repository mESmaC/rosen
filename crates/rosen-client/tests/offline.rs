//! SPEC R2 critical contract: intermediate offline edits survive restart and
//! replay in order, even when the file has subsequently been overwritten.
use rosen_client::protocol::{Listener, NewChange};
use rosen_client::snapshot::{
    journal_snapshot, pending_journal, take_snapshot, Dirty, Snapshotter,
};
use rosen_client::workspace::Options;
use rosen_client::{Client, Endpoint};
use rosen_core::{AgentId, FileMode, ObjectId, Tree, TreeEntry};
use rosen_server::changes::{BaseSource, BaseTip};
use rosen_server::{ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::{path::Path, sync::Arc};
struct FixedBase(BaseTip);
impl BaseSource for FixedBase {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(self.0)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_versions_survive_restart_and_replay_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let store = LooseObjectStore::open(data.join("objects")).unwrap();
    let endpoint = start_server(&data, &store);
    let client = Client::connect(&endpoint).await.unwrap();
    let change = client
        .change_new(NewChange {
            owner: AgentId::new("offline").unwrap(),
            on: None,
        })
        .await
        .unwrap();
    let workspace = dir.path().join("ws");
    client
        .materialise_change(&change, &workspace, Options::default())
        .await
        .unwrap();
    let mut state = Snapshotter::open_offline(&workspace).unwrap();
    for content in [b"first offline edit".as_slice(), b"second offline edit"] {
        std::fs::write(workspace.join("main.rs"), content).unwrap();
        let (back, result) = journal_snapshot(state, Dirty::Full).await;
        state = back;
        assert!(matches!(result, Err(rosen_client::Error::Disconnected)));
    }
    assert_eq!(pending_journal(&workspace).unwrap(), 2);
    drop(state);
    let state = Snapshotter::open_offline(&workspace).unwrap();
    // A new write after restart must follow both durable intermediate versions.
    std::fs::write(workspace.join("main.rs"), b"third edit").unwrap();
    let (state, result) = take_snapshot(&client, state, Dirty::Full, None).await;
    result.unwrap();
    drop(state);
    assert_eq!(pending_journal(&workspace).unwrap(), 0);
    verify_history(&client, &store, &change.change.id.to_string(), &workspace).await;
    let lease = Snapshotter::open_offline(&workspace).unwrap();
    let deleting = dir.path().join("deleting");
    std::fs::rename(&workspace, &deleting).unwrap();
    std::fs::remove_dir_all(deleting).unwrap();
    drop(lease);
}

async fn verify_history(client: &Client, store: &dyn ObjectStore, change: &str, workspace: &Path) {
    let history = client.change_show(change).await.unwrap();
    let versions: Vec<_> = history
        .change
        .snapshots
        .iter()
        .map(|snapshot| {
            let tree = store.read_tree(&snapshot.tree).unwrap();
            store
                .read_blob(&tree.get(b"main.rs").unwrap().id)
                .unwrap()
                .data
        })
        .collect();
    assert_eq!(
        versions,
        vec![
            b"first offline edit".to_vec(),
            b"second offline edit".to_vec(),
            b"third edit".to_vec()
        ]
    );
    client.snapshot_workspace(workspace, None).await.unwrap();
    assert_eq!(
        client
            .change_show(change)
            .await
            .unwrap()
            .change
            .snapshots
            .len(),
        3
    );
}

/// An in-process server whose base holds `main.rs` and a `.gitignore`
/// ignoring `build/`.
fn start_server(data: &Path, store: &dyn ObjectStore) -> Endpoint {
    let entries = vec![
        file(store, ".gitignore", b"build/\n"),
        file(store, "main.rs", b"fn main() {}\n"),
    ];
    let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
    let name = format!("rosen-offline-test-{}", std::process::id());
    let endpoint = if cfg!(windows) {
        Endpoint::new(name)
    } else {
        Endpoint::new(data.join("s.sock").to_string_lossy())
    };
    let listener = Listener::bind(&endpoint).unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.to_path_buf(),
        endpoint: endpoint.clone(),
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
