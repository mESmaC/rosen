//! SPEC.md §5 contract: Velocity creates, watches and deletes workspaces and
//! claims paths through `rosen-client` alone (D7).
//!
//! Risk: the session is the one surface Velocity links. If it creates a
//! change under the wrong owner, makes a workspace whose writes never reach
//! the server, or leaves a watcher running that stops deletion, every agent
//! thread Velocity launches is affected at once.

use rosen_client::protocol::{ClaimOutcome, Listener};
use rosen_client::{AgentId, Endpoint, Error, Session, WatchEvent, WorkspaceSpec};
use rosen_core::{FileMode, ObjectId, Tree, TreeEntry};
use rosen_server::changes::{BaseSource, BaseTip};
use rosen_server::{ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn a_session_runs_a_workspace_from_creation_to_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let endpoint = start_server(&data);
    let agent = AgentId::new("session-test").unwrap();
    let session = Session::builder()
        .endpoint(endpoint)
        .agent(agent.clone())
        .start_daemon(false)
        .open()
        .await
        .unwrap();

    let root = dir.path().join("workspace");
    let created = session
        .create_workspace(WorkspaceSpec::new(&root))
        .await
        .unwrap();
    assert_eq!(created.change.change.owner, agent);
    let change = created.change.change.id.to_string();

    let claimed = session.claim(&change, &["main.rs".into()]).await.unwrap();
    assert!(
        matches!(claimed, ClaimOutcome::Granted { .. }),
        "{claimed:?}"
    );

    let mut watch = session.watch(&root);
    std::fs::write(root.join("main.rs"), b"fn main() { edited() }\n").unwrap();
    let stored = wait_for_snapshot(&mut watch, "main.rs").await;
    watch.stop().await.unwrap();
    let head = session.client().change_show(&change).await.unwrap();
    assert_eq!(head.change.head, stored);

    // A second create into the same directory fails, but names the change
    // it made so Velocity can retry or abandon it.
    let again = session.create_workspace(WorkspaceSpec::new(&root)).await;
    assert!(matches!(again, Err(Error::WorkspaceNotCreated { .. })));

    session.delete_workspace(&root).await.unwrap();
    assert!(!root.exists());
}

/// The tree of the first snapshot that carried `path`.
async fn wait_for_snapshot(watch: &mut rosen_client::WorkspaceWatch, path: &str) -> ObjectId {
    let deadline = Duration::from_secs(20);
    tokio::time::timeout(deadline, async {
        loop {
            match watch.next_report().await.expect("watcher ended") {
                WatchEvent::Snapshot(report) if report.changed_files.iter().any(|f| f == path) => {
                    return report.tree;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("no snapshot within the deadline")
}

struct FixedBase(BaseTip);
impl BaseSource for FixedBase {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(self.0)
    }
}

fn start_server(data: &Path) -> Endpoint {
    let store = LooseObjectStore::open(data.join("objects")).unwrap();
    let blob = store.put_blob(b"fn main() {}\n").unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "main.rs", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
    let endpoint = if cfg!(windows) {
        Endpoint::new(format!("rosen-session-test-{}", std::process::id()))
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
    let base = BaseTip { commit, tree };
    state.changes.set_base_source(Arc::new(FixedBase(base)));
    tokio::spawn(rosen_server::listener::serve(listener, state));
    endpoint
}
