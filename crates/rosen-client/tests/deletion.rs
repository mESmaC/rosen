//! SPEC.md R4 critical contract: deleting a workspace must not lose an edit.
//!
//! Risk: the snapshot window and a closed change can leave bytes only on
//! disk. Cleanup must store them first or name them and keep the workspace.
use rosen_client::protocol::{ChangeRecord, Listener, NewChange, Request};
use rosen_client::snapshot::{journal_snapshot, pending_journal, Dirty, Snapshotter};
use rosen_client::workspace::{deletion_permitted, pool::WarmPool, Options};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::{AgentId, FileMode, ObjectId, Tree, TreeEntry};
use rosen_server::changes::{BaseSource, BaseTip};
use rosen_server::{ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn deletion_stores_edits_or_keeps_and_names_them() {
    check_design_fixtures();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let store = LooseObjectStore::open(data.join("objects")).unwrap();
    let endpoint = start_server(&data, &store);
    let client = Client::connect(&endpoint).await.unwrap();
    abandon_stores_edits(&client, &store, &dir.path().join("workspace")).await;
    closed_edits_survive(&client, dir.path()).await;
    active_lease_keeps_workspace(&client, &dir.path().join("leased")).await;
    offline_journal_keeps_workspace(&client, &dir.path().join("journalled")).await;
    #[cfg(windows)]
    writable_handle_keeps_workspace(&client, &dir.path().join("writable")).await;
    #[cfg(windows)]
    interrupted_cleanup_finishes(&client, &data, &dir.path().join("interrupted")).await;
    daemon_cleans_closed_workspace(&client, &endpoint, &data, &dir.path().join("automatic")).await;
}

async fn abandon_stores_edits(client: &Client, store: &dyn ObjectStore, workspace: &Path) {
    let change = new_workspace(client, workspace).await;
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
}

async fn closed_edits_survive(client: &Client, root: &Path) {
    let kept = root.join("kept");
    let sibling = root.join("sibling");
    let closed = new_workspace(client, &kept).await;
    let _sibling_change = new_workspace(client, &sibling).await;
    std::fs::write(kept.join("main.rs"), b"unsaved edit\n").unwrap();
    close_without_snapshot(client, &closed).await;
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
        b"unsaved edit\n"
    );
    assert!(
        sibling.join("main.rs").exists(),
        "cleanup touched another workspace"
    );
}

async fn active_lease_keeps_workspace(client: &Client, workspace: &Path) {
    new_workspace(client, workspace).await;
    let lease = Snapshotter::open_offline(workspace).unwrap();
    client.workspace_delete(workspace).await.unwrap_err();
    assert!(
        workspace.join("main.rs").exists(),
        "active watcher lease was ignored"
    );
    drop(lease);
}

async fn offline_journal_keeps_workspace(client: &Client, workspace: &Path) {
    let change = new_workspace(client, workspace).await;
    let original = std::fs::read(workspace.join("main.rs")).unwrap();
    let state = Snapshotter::open_offline(workspace).unwrap();
    std::fs::write(workspace.join("main.rs"), b"intermediate offline edit\n").unwrap();
    let (state, result) = journal_snapshot(state, Dirty::Full).await;
    assert!(matches!(result, Err(Error::Disconnected)));
    drop(state);
    assert_eq!(pending_journal(workspace).unwrap(), 1);
    // A matching disk hash does not make the overwritten intermediate edit
    // durable: its unacknowledged journal entry must still keep the workspace.
    std::fs::write(workspace.join("main.rs"), &original).unwrap();
    close_without_snapshot(client, &change).await;
    let refusal = client
        .workspace_delete(workspace)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("main.rs"),
        "journal refusal did not name the file: {refusal}"
    );
    assert_eq!(pending_journal(workspace).unwrap(), 1);
    assert_eq!(std::fs::read(workspace.join("main.rs")).unwrap(), original);
}

#[cfg(windows)]
async fn writable_handle_keeps_workspace(client: &Client, workspace: &Path) {
    let change = new_workspace(client, workspace).await;
    let original = std::fs::read(workspace.join("main.rs")).unwrap();
    // Keep disk identical to the snapshot: only the live writable handle
    // should prevent deletion, even after the directory has been staged.
    let writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(workspace.join("main.rs"))
        .unwrap();
    close_without_snapshot(client, &change).await;
    client.workspace_delete(workspace).await.unwrap_err();
    let parent = workspace.parent().unwrap();
    let stage_prefix = format!(".rosen-delete-{}-", change.change.id);
    let kept = std::iter::once(workspace.to_path_buf())
        .chain(
            std::fs::read_dir(parent)
                .unwrap()
                .map(|entry| entry.unwrap().path()),
        )
        .find(|path| {
            (path == workspace
                || path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&stage_prefix)))
                && path.join("main.rs").exists()
        })
        .expect("deletion removed a file with a live writable handle");
    assert_eq!(std::fs::read(kept.join("main.rs")).unwrap(), original);
    drop(writer);
}
/// Issue #90: a crash after verified removal deleted some tracked files of
/// the staged directory must not leave a cleanup that refuses forever.
#[cfg(windows)]
async fn interrupted_cleanup_finishes(client: &Client, data: &Path, workspace: &Path) {
    let change = new_workspace(client, workspace).await;
    close_without_snapshot(client, &change).await;
    // The registration file's name is the identity that names the staging
    // directory beside the workspace.
    let identity = std::fs::read_dir(data.join("workspaces"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && std::fs::read_to_string(path)
                    .unwrap()
                    .contains(&change.change.id.to_string())
        })
        .expect("workspace was not registered");
    let staged = workspace.with_file_name(format!(
        ".rosen-delete-{}-{}",
        change.change.id,
        identity.file_stem().unwrap().to_string_lossy()
    ));
    std::fs::rename(workspace, &staged).unwrap();
    std::fs::remove_file(staged.join("main.rs")).unwrap();
    // Earlier scenarios' kept workspaces are refused in the same pass.
    let reports = client.cleanup_workspaces().await.unwrap();
    let report = reports
        .iter()
        .find(|report| report.path.ends_with(workspace.file_name().unwrap()))
        .expect("interrupted cleanup was not attempted");
    assert!(
        report.error.is_none(),
        "interrupted cleanup was refused: {report:?}"
    );
    assert!(!staged.exists(), "staged workspace was kept");
}

async fn daemon_cleans_closed_workspace(
    client: &Client,
    endpoint: &Endpoint,
    data: &Path,
    path: &Path,
) {
    let baseline = client.status().await.unwrap().open_connections;
    let pool = WarmPool::new(data);
    pool.set_size(0).await;
    let worker = pool.start(endpoint.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.status().await.unwrap().open_connections <= baseline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon worker never connected");
    let change = new_workspace(client, path).await;
    close_without_snapshot(client, &change).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon did not clean the closed workspace after its event");
    worker.abort();
    let _ = worker.await;
}

async fn close_without_snapshot(client: &Client, change: &ChangeRecord) {
    client
        .request(Request::ChangeAbandon {
            change: change.change.id.to_string(),
        })
        .await
        .unwrap();
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
        warm_pool_dir: None,
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
