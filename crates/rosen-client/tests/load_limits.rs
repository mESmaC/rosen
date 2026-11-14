use rosen_client::workspace::{
    limits::{Budget, Settings},
    materialise_with_budget, Options,
};
use rosen_core::{FileMode, Tree, TreeEntry};
use rosen_store::{LooseObjectStore, ObjectStore};
use std::{fs, sync::mpsc, time::Duration};

// SPEC R3: over-cap work must wait and preserve existing workspaces.
// Risk: concurrent clients can otherwise exhaust the machine disk or corrupt a workspace.
#[test]
fn disk_admission_queues_and_resumes_without_touching_existing_workspaces() {
    let root = tempfile::tempdir().unwrap();
    let store = LooseObjectStore::open(root.path().join("objects")).unwrap();
    let blob = store.put_blob(b"four").unwrap();
    let tree = store
        .put_tree(
            &Tree::new(vec![
                TreeEntry::new(FileMode::Regular, "file", blob).unwrap()
            ])
            .unwrap(),
        )
        .unwrap();
    let limits = root.path().join("limits");
    let budget = Budget::open(
        &limits,
        Settings {
            materialisations: 1,
            disk_bytes: 4,
        },
    )
    .unwrap();
    let first = root.path().join("first");
    materialise_with_budget(&store, tree, &first, &Options::default(), Some(&budget)).unwrap();
    let taken = root.path().join("taken");
    budget.rename_workspace(&first, &taken).unwrap();
    let first = taken;
    let second = root.path().join("second");
    let second_copy = second.clone();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let budget = Budget::open(&limits, Settings::default()).unwrap();
        materialise_with_budget(
            &store,
            tree,
            &second_copy,
            &Options::default(),
            Some(&budget),
        )
        .unwrap();
        tx.send(()).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    assert!(!second.exists());
    assert_eq!(fs::read(first.join("file")).unwrap(), b"four");
    budget.set_disk_cap(8).unwrap();
    rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    assert_eq!(fs::read(second.join("file")).unwrap(), b"four");
}

// SPEC R3: workspace I/O must queue while ordinary requests stay responsive.
// Risk: a handler that bypasses admission can exhaust server resources.
#[tokio::test]
async fn server_writes_wait_for_io_capacity_while_ping_still_answers() {
    use rosen_client::protocol::{Endpoint, Outcome, Request, Response, WorkspaceWrite};
    use rosen_server::{rpc::dispatch, ServerConfig, ServerState};
    use std::{num::NonZeroUsize, sync::Arc};
    let root = tempfile::tempdir().unwrap();
    let state = ServerState::with_workspace_io(
        ServerConfig {
            data_dir: root.path().into(),
            endpoint: Endpoint::new("load-contract"),
            warm_pool_dir: None,
        },
        NonZeroUsize::new(1).unwrap(),
    );
    let held = Arc::clone(&state.workspace_io)
        .acquire_owned()
        .await
        .unwrap();
    let writer = Arc::clone(&state);
    let mut pending = tokio::spawn(async move {
        dispatch(
            &writer,
            Request::WorkspaceWrite(WorkspaceWrite {
                change: "missing".into(),
                path: "file".into(),
                data: None,
            }),
        )
        .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut pending)
            .await
            .is_err()
    );
    assert_eq!(
        dispatch(&state, Request::Ping).await,
        Outcome::Ok(Response::Pong)
    );
    drop(held);
    let answer = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    // The missing change is examined only after admission, then fails normally.
    assert!(matches!(answer, Outcome::Error(_)));
}
