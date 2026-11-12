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
