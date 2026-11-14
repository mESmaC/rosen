//! Risk (SPEC R2): a server rebase must keep commit IDs and must not replace
//! local files until the owner requests it; unsnapshotted bytes must survive.
use rosen_client::{snapshot::WorkspaceMarker, workspace::Options, Client};
use rosen_core::{AgentId, Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_protocol::{Endpoint, Listener, NewChange};
use rosen_server::{landing::Ledger, ServerConfig, ServerState};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::sync::Arc;

fn tree(store: &dyn ObjectStore, extra: bool) -> ObjectId {
    let mut files = vec![
        TreeEntry::new(
            FileMode::Regular,
            b"a".to_vec(),
            store.put_blob(b"a\n").unwrap(),
        )
        .unwrap(),
        TreeEntry::new(
            FileMode::Regular,
            b".gitignore".to_vec(),
            store.put_blob(b"build/\n").unwrap(),
        )
        .unwrap(),
    ];
    if extra {
        files.push(
            TreeEntry::new(
                FileMode::Regular,
                b"b".to_vec(),
                store.put_blob(b"b\n").unwrap(),
            )
            .unwrap(),
        );
    }
    store.put_tree(&Tree::new(files).unwrap()).unwrap()
}

fn commit(store: &dyn ObjectStore, tree: ObjectId, parents: Vec<ObjectId>) -> ObjectId {
    let who = Signature::new("test", "test@test", 0);
    store
        .put_commit(&Commit {
            tree,
            parents,
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"base\n".to_vec(),
        })
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn server_rebase_preserves_history_and_requires_explicit_clean_apply() {
    let temp = tempfile::tempdir().unwrap();
    let (state, store, client, server, original, base) = start(&temp.path().join("data")).await;
    let owner = AgentId::new("owner").unwrap();
    let change = client
        .change_new(NewChange {
            owner: owner.clone(),
            on: None,
        })
        .await
        .unwrap();
    let root = temp.path().join("workspace");
    client
        .materialise_change(&change, &root, Options::default())
        .await
        .unwrap();
    std::fs::write(root.join("a"), b"authored\n").unwrap();
    client
        .snapshot_workspace(&root, Some("author commit".into()))
        .await
        .unwrap();
    let id = change.change.id.to_string();
    let before = client.change_show(&id).await.unwrap();
    let next = commit(&store, tree(&store, true), vec![base]);
    // Simulate an independent land, so this test exercises only the R2 contract.
    state
        .changes
        .with_conn(|conn| {
            conn.execute("UPDATE base_refs SET target = ?1", [next.to_hex()])?;
            Ok(())
        })
        .unwrap();
    let rebased = client.rebase_change(&id).await.unwrap();
    assert_eq!(rebased.change.snapshots, before.change.snapshots);
    assert_eq!(rebased.change.parent, rosen_core::ChangeParent::Base(next));
    assert!(!root.join("b").exists());
    assert_eq!(
        WorkspaceMarker::read(&root).unwrap().tree,
        before.change.head
    );
    let head = snapshot_during_pending_rebase(&client, &root, &id).await;
    check_apply(&client, &root, &owner, head).await;
    // An obsolete merge result cannot overwrite a newer head.
    assert!(state
        .changes
        .with_conn(|conn| rosen_server::changes::rebase_checked(
            conn,
            &before.change,
            next,
            original
        ))
        .is_err());
    server.abort();
}

async fn check_apply(client: &Client, root: &std::path::Path, owner: &AgentId, tree: ObjectId) {
    assert!(client
        .apply_rebase(root, &AgentId::new("other").unwrap())
        .await
        .is_err());
    std::fs::write(root.join("a"), b"unstored\n").unwrap();
    assert!(client.apply_rebase(root, owner).await.is_err());
    assert_eq!(std::fs::read(root.join("a")).unwrap(), b"unstored\n");
    std::fs::write(root.join("a"), b"step two\n").unwrap();
    std::fs::create_dir(root.join("build")).unwrap();
    std::fs::write(root.join("build/output"), b"keep").unwrap();
    assert_eq!(client.apply_rebase(root, owner).await.unwrap(), tree);
    assert_eq!(std::fs::read(root.join("a")).unwrap(), b"step two\n");
    assert_eq!(std::fs::read(root.join("b")).unwrap(), b"b\n");
    assert_eq!(std::fs::read(root.join("build/output")).unwrap(), b"keep");
    assert_eq!(WorkspaceMarker::read(root).unwrap().tree, tree);
}

async fn start(
    data: &std::path::Path,
) -> (
    Arc<ServerState>,
    LooseObjectStore,
    Client,
    tokio::task::JoinHandle<()>,
    ObjectId,
    ObjectId,
) {
    let endpoint = Endpoint::new(format!("rebase-{}", std::process::id()));
    let state = ServerState::new(ServerConfig {
        data_dir: data.to_path_buf(),
        endpoint: endpoint.clone(),
        warm_pool_dir: None,
    });
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let original = tree(&store, false);
    let base = commit(&store, original, vec![]);
    Ledger::open(&state.config.metadata_path())
        .unwrap()
        .init_base("main", &base)
        .unwrap();
    let listener = Listener::bind(&endpoint).unwrap();
    let server = tokio::spawn(rosen_server::listener::serve(listener, Arc::clone(&state)));
    let client = Client::connect(&endpoint).await.unwrap();
    (state, store, client, server, original, base)
}

// Regression from PR duty: the watcher may snapshot while the agent finishes
// its step on the old local tree. The pending base additions must survive.
async fn snapshot_during_pending_rebase(
    client: &Client,
    root: &std::path::Path,
    id: &str,
) -> ObjectId {
    std::fs::write(root.join("a"), b"step two\n").unwrap();
    client.snapshot_workspace(root, None).await.unwrap();
    assert!(!root.join("b").exists());
    let change = client.change_show(id).await.unwrap();
    let result = client.workspace_read(id, "b").await.unwrap();
    assert!(matches!(result, rosen_protocol::ReadAnswer::File { .. }));
    change.change.head
}
