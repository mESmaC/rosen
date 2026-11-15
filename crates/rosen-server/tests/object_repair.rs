//! Risk (SPEC D5/D8/R2): a confirmed unchanged blob or a descendant of a
//! complete cache tree can otherwise remain corrupt forever (#363).
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rosen_client::snapshot::{take_snapshot, Dirty};
use rosen_client::{workspace::Options, Client, Endpoint};
use rosen_core::{AgentId, FileMode, ObjectId, Tree, TreeEntry};
use rosen_protocol::{Listener, NewChange, ObjectQuery, Request};
use rosen_server::{
    changes::{BaseSource, BaseTip},
    network, ServerConfig, ServerState,
};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::{collections::BTreeSet, path::Path, sync::Arc};

struct Base(BaseTip);
impl BaseSource for Base {
    fn base_tip(&self) -> Result<BaseTip, String> {
        Ok(self.0)
    }
}

fn config(dir: &Path) -> ServerConfig {
    std::fs::create_dir_all(dir).unwrap();
    let endpoint = if cfg!(windows) {
        Endpoint::new(format!(
            "repair-{}-{}-{}",
            std::process::id(),
            dir.parent().unwrap().file_name().unwrap().to_string_lossy(),
            dir.file_name().unwrap().to_string_lossy(),
        ))
    } else {
        Endpoint::new(dir.join("s.sock").to_string_lossy())
    };
    ServerConfig {
        data_dir: dir.into(),
        endpoint,
        warm_pool_dir: None,
    }
}

fn project(dir: &Path, bytes: &[u8]) -> (Arc<ServerState>, LooseObjectStore, ObjectId, ObjectId) {
    let state = ServerState::new(config(dir));
    let store = LooseObjectStore::open(state.config.objects_dir()).unwrap();
    let blob = store.put_blob(bytes).unwrap();
    let sub = Tree::new(vec![
        TreeEntry::new(FileMode::Regular, "file", blob).unwrap()
    ])
    .unwrap();
    let sub = store.put_tree(&sub).unwrap();
    let root = Tree::new(vec![
        TreeEntry::new(FileMode::Directory, "dir", sub).unwrap()
    ])
    .unwrap();
    let root = store.put_tree(&root).unwrap();
    state.changes.set_base_source(Arc::new(Base(BaseTip {
        tree: root,
        commit: ObjectId::hash_object("commit", b"base"),
    })));
    (state, store, root, blob)
}

fn damage(store: &LooseObjectStore, id: ObjectId) {
    std::fs::write(store.object_path(&id), b"damaged object").unwrap();
    assert!(!store.contains_valid(&id).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_blobs_are_repaired_and_newer_bytes_are_never_relabelled() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = vec![b'x'; rosen_protocol::OBJECT_CHUNK_BYTES + 1];
    let (state, store, root, blob) = project(&temp.path().join("server"), &bytes);
    let listener = Listener::bind(&state.config.endpoint).unwrap();
    tokio::spawn(rosen_server::listener::serve(listener, Arc::clone(&state)));
    let client = Client::connect(&state.config.endpoint).await.unwrap();
    let change = client
        .change_new(NewChange {
            owner: AgentId::new("writer").unwrap(),
            on: None,
        })
        .await
        .unwrap();
    let workspace = temp.path().join("ws");
    client
        .materialise_change(&change, &workspace, Options::default())
        .await
        .unwrap();
    let writer = client.open_snapshotter(&workspace).await.unwrap();
    damage(&store, blob);
    sparse_repair_preserves_omitted_paths(&client, &change, temp.path()).await;
    let dirty = || Dirty::Paths(BTreeSet::new());
    let (writer, report) = take_snapshot(&client, writer, dirty(), None).await;
    assert_eq!(report.unwrap().sent.blobs, 1);
    assert_eq!(store.read_blob(&blob).unwrap().data, bytes);
    damage(&store, root);
    let (writer, report) = take_snapshot(&client, writer, dirty(), None).await;
    assert_eq!(report.unwrap().sent.trees, 1);
    damage(&store, blob);
    std::fs::write(workspace.join("dir/file"), b"newer bytes").unwrap();
    let (writer, report) = take_snapshot(&client, writer, dirty(), None).await;
    assert!(report.is_err(), "newer bytes cannot repair the older blob");
    assert!(!store.contains_valid(&blob).unwrap());
    let (writer, report) = take_snapshot(&client, writer, dirty(), None).await;
    assert!(
        report.unwrap().recorded,
        "the repair hint forces the changed path to be read"
    );
    #[cfg(windows)]
    repair_bytes_survive_failed_replay(&client, writer, &store).await;
    #[cfg(not(windows))]
    drop(writer);
    state.request_shutdown();
}

#[cfg(windows)]
async fn repair_bytes_survive_failed_replay(
    client: &Client,
    writer: rosen_client::snapshot::Snapshotter,
    store: &LooseObjectStore,
) {
    use std::os::windows::fs::OpenOptionsExt;
    let workspace = writer.root().to_path_buf();
    std::fs::write(workspace.join("extra"), b"queued offline").unwrap();
    let (writer, queued) = rosen_client::snapshot::journal_snapshot(writer, Dirty::Full).await;
    assert!(queued.is_err());
    let blob = ObjectId::hash_object("blob", b"newer bytes");
    damage(store, blob);
    // Allow verification reads but refuse replacement, so repair fails only
    // after rereading the unchanged file and must retain it in the journal.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(store.object_path(&blob))
        .unwrap();
    let (writer, failed) = take_snapshot(client, writer, Dirty::Paths(BTreeSet::new()), None).await;
    assert!(failed.is_err());
    drop(writer);
    std::fs::write(workspace.join("dir/file"), b"latest bytes").unwrap();
    drop(lock);
    let writer = client.open_snapshotter(&workspace).await.unwrap();
    let (_, result) = take_snapshot(client, writer, Dirty::Full, None).await;
    assert!(result.is_ok());
    assert_eq!(store.read_blob(&blob).unwrap().data, b"newer bytes");
}

async fn sparse_repair_preserves_omitted_paths(
    client: &Client,
    change: &rosen_protocol::ChangeRecord,
    dir: &Path,
) {
    // An omitted file is not a local deletion, even after a repair failed.
    let sparse = dir.join("sparse");
    let options = Options {
        sparse: rosen_client::workspace::Sparse::paths(["absent"]).unwrap(),
        ..Options::default()
    };
    client
        .materialise_change(change, &sparse, options)
        .await
        .unwrap();
    let writer = client.open_snapshotter(&sparse).await.unwrap();
    let dirty = || Dirty::Paths(BTreeSet::new());
    let (writer, result) = take_snapshot(client, writer, dirty(), None).await;
    assert!(result.is_err());
    let (_, result) = take_snapshot(client, writer, dirty(), None).await;
    assert!(
        result.is_err(),
        "an unavailable off-sparse blob must stay indexed"
    );
}

async fn upstream(state: &Arc<ServerState>) -> network::UpstreamConfig {
    let identity = network::Identity::load_or_create(&state.config.data_dir).unwrap();
    let pin = identity.fingerprint();
    let acceptor = network::tls::acceptor(&identity).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    state.set_network(identity, address);
    let key = rosen_server::machines::keys::create(
        state,
        rosen_protocol::CreateJoinKey {
            address: address.to_string(),
            expires_at: rosen_server::machines::now() + 3600,
            use_limit: 1,
            role: rosen_protocol::MachineRole::Agent,
        },
    )
    .unwrap();
    let key: rosen_protocol::JoinKey =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(key).unwrap()).unwrap();
    let credential = rosen_server::machines::credentials::enroll(
        &state.config.data_dir,
        &key.secret,
        "cache",
        None,
    )
    .unwrap();
    tokio::spawn(network::serve(tcp, acceptor, Arc::clone(state)));
    network::UpstreamConfig {
        address: address.to_string(),
        fingerprint: pin,
        credential: credential.secret,
    }
}

async fn connect(endpoint: &Endpoint) -> Client {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if let Ok(client) = Client::connect(endpoint).await {
                return client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn complete_cache_descendants_and_read_through_are_repaired() {
    let temp = tempfile::tempdir().unwrap();
    let (state, _, root, blob) = project(&temp.path().join("server"), b"held bytes");
    let upstream = upstream(&state).await;
    let config = config(&temp.path().join("cache"));
    let endpoint = config.endpoint.clone();
    let store = LooseObjectStore::open(config.objects_dir()).unwrap();
    tokio::spawn(async move {
        rosen_server::cache::serve(&config, upstream).await.unwrap();
    });
    let client = connect(&endpoint).await;
    client.request(Request::PoolSize { size: 0 }).await.unwrap();
    client.ensure_local(root).await.unwrap();
    damage(&store, blob);
    client.ensure_local(root).await.unwrap();
    assert_eq!(store.read_blob(&blob).unwrap().data, b"held bytes");
    damage(&store, blob);
    client
        .request(Request::GetObjects(ObjectQuery { ids: vec![blob] }))
        .await
        .unwrap();
    assert_eq!(store.read_blob(&blob).unwrap().data, b"held bytes");
    damage(&store, root);
    client
        .request(Request::GetObjects(ObjectQuery { ids: vec![root] }))
        .await
        .unwrap();
    assert!(store.read_tree(&root).is_ok());
    client.request(Request::Shutdown).await.unwrap();
    state.request_shutdown();
}
