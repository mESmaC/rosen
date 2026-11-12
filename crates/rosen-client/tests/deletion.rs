//! SPEC.md R4 critical contract: deleting a workspace must not lose an edit.
//!
//! Risk: the snapshot window and a closed change can leave bytes only on
//! disk. Cleanup must store them first or name them and keep the workspace.
use rosen_client::protocol::{ChangeRecord, Listener, NewChange, Request};
use rosen_client::workspace::{deletion_permitted, Options};
use rosen_client::{Client, Endpoint};
use rosen_core::{AgentId, FileMode, ObjectId, Tree, TreeEntry};
use rosen_server::changes::{BaseSource, BaseTip};
use rosen_server::{ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::path::Path;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn deletion_stores_edits_or_keeps_and_names_them() {
    check_design_fixtures();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let store = LooseObjectStore::open(data.join("objects")).unwrap();
    let endpoint = start_server(&data, &store);
    let client = Client::connect(&endpoint).await.unwrap();
    let workspace = dir.path().join("workspace");
    let change = new_workspace(&client, &workspace).await;
    std::fs::write(workspace.join("main.rs"), b"durable edit\n").unwrap();
    std::fs::create_dir(workspace.join("build")).unwrap();
    std::fs::write(workspace.join("build/output"), b"ignored").unwrap();
    let abandoned = client
        .change_abandon(&change.change.id.to_string())
        .await
        .unwrap();
    assert!(
        !workspace.exists(),
        "abandon did not clean up the workspace"
    );
    let tree = store.read_tree(&abandoned.change.head).unwrap();
    let edited = tree.get(b"main.rs").unwrap();
    assert_eq!(store.read_blob(&edited.id).unwrap().data, b"durable edit\n");
    assert!(tree.get(b"build").is_none(), "ignored output was uploaded");

    let kept = dir.path().join("kept");
    let sibling = dir.path().join("sibling");
    let closed = new_workspace(&client, &kept).await;
    let _sibling_change = new_workspace(&client, &sibling).await;
    std::fs::write(kept.join("main.rs"), b"after close\n").unwrap();
    client
        .request(Request::ChangeAbandon {
            change: closed.change.id.to_string(),
        })
        .await
        .unwrap();
    std::fs::write(kept.join("main.rs"), b"after close\n").unwrap();
    let refusal = client
        .workspace_delete(&kept)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("main.rs"),
        "refusal did not name the file: {refusal}"
    );
    assert_eq!(
        std::fs::read(kept.join("main.rs")).unwrap(),
        b"after close\n"
    );
    assert!(
        sibling.join("main.rs").exists(),
        "cleanup touched another workspace"
    );
}

fn check_design_fixtures() {
    for line in include_str!("../../../proofs/fixtures/workspace-deletion.tsv").lines() {
        if line.is_empty() || line.starts_with('#') || line.starts_with("dirty") {
            continue;
        }
        let columns: Vec<_> = line.split('\t').collect();
        assert_eq!(columns.len(), 5, "malformed deletion fixture: {line}");
        let values: Vec<_> = columns
            .iter()
            .map(|value| match *value {
                "1" | "true" => true,
                "0" | "false" => false,
                other => panic!("invalid fixture value: {other}"),
            })
            .collect();
        let mismatch = usize::from(values[2] && !values[3]);
        assert_eq!(
            deletion_permitted(usize::from(values[0]), usize::from(values[1]), mismatch),
            values[4],
            "{line}"
        );
    }
}

async fn new_workspace(client: &Client, path: &Path) -> ChangeRecord {
    let owner = AgentId::new("deletion-test").unwrap();
    let change = client
        .change_new(NewChange { owner, on: None })
        .await
        .unwrap();
    client
        .materialise_change(&change, path, Options::default())
        .await
        .unwrap();
    change
}

struct FixedBase(BaseTip);
impl BaseSource for FixedBase {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(self.0)
    }
}

fn start_server(data: &Path, store: &dyn ObjectStore) -> Endpoint {
    let entries = vec![
        file(store, ".gitignore", b"build/\n"),
        file(store, "main.rs", b"fn main() {}\n"),
    ];
    let tree = store.put_tree(&Tree::new(entries).unwrap()).unwrap();
    let endpoint = if cfg!(windows) {
        Endpoint::new(format!("rosen-deletion-test-{}", std::process::id()))
    } else {
        Endpoint::new(data.join("s.sock").to_string_lossy())
    };
    let listener = Listener::bind(&endpoint).unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.to_path_buf(),
        endpoint: endpoint.clone(),
    });
    let commit = ObjectId::hash_object("commit", b"base");
    state
        .changes
        .set_base_source(Arc::new(FixedBase(BaseTip { commit, tree })));
    tokio::spawn(rosen_server::listener::serve(listener, state));
    endpoint
}

fn file(store: &dyn ObjectStore, name: &str, bytes: &[u8]) -> TreeEntry {
    let blob = store.put_blob(bytes).unwrap();
    TreeEntry::new(FileMode::Regular, name, blob).unwrap()
}
