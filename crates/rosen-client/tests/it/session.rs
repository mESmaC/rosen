//! SPEC.md §5 contract: Velocity creates, watches and deletes workspaces and
//! claims paths through `rosen-client` alone (D7).
//!
//! Risk: the session is the one surface Velocity links. If it creates a
//! change under the wrong owner, makes a workspace whose writes never reach
//! the server, or leaves a watcher running that stops deletion, every agent
//! thread Velocity launches is affected at once.

use rosen_client::protocol::{ClaimOutcome, Listener};
use rosen_client::workspace::{Method, Options};
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
    let endpoint = start_server(&data, "lifecycle");
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

/// Risk (spec/R3-workspaces/warm-pool.md): a caller that gave up on a busy
/// pool asks for its workspace without it. If the client still sent a take,
/// that retry would queue behind the pool again (#426).
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_without_the_pool_sends_no_take() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let session = Session::builder()
        .endpoint(start_server(&data, "no-pool"))
        .agent(AgentId::new("session-test").unwrap())
        .start_daemon(false)
        .open()
        .await
        .unwrap();
    // The pool refuses every take into its own ready root, so a refusal
    // shows a take was sent and a workspace there shows none was.
    let ready_root = data.join("warm-pool");
    std::fs::create_dir_all(&ready_root).unwrap();
    let asked = WorkspaceSpec::new(ready_root.join("asked"));
    let Err(Error::WorkspaceNotCreated { change, .. }) = session.create_workspace(asked).await
    else {
        panic!("the pool did not refuse a take into its ready root");
    };

    let (retried, created) = (ready_root.join("retried"), ready_root.join("created"));
    let options = Options {
        skip_pool: true,
        ..Options::default()
    };
    let report = session
        .open_workspace_with(&change.to_string(), &retried, options)
        .await
        .unwrap();
    assert_eq!(report.method, Method::PlainCopy);
    assert!(retried.join("main.rs").is_file());
    let spec = WorkspaceSpec::new(&created).without_pool();
    session.create_workspace(spec).await.unwrap();
    assert!(created.join("main.rs").is_file());
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

/// `name` keeps this file's tests, which share a process, on separate pipes.
fn start_server(data: &Path, name: &str) -> Endpoint {
    let store = LooseObjectStore::open(data.join("objects")).unwrap();
    let blob = store.put_blob(b"fn main() {}\n").unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "main.rs", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
    let endpoint = if cfg!(windows) {
        Endpoint::new(format!("rosen-session-test-{name}-{}", std::process::id()))
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
    let base = BaseTip { commit, tree };
    state.changes.set_base_source(Arc::new(FixedBase(base)));
    tokio::spawn(rosen_server::listener::serve(listener, state));
    endpoint
}
