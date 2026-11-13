//! SPEC.md R3: hardlinks are never used for files in a workspace.
//!
//! Risk: agents' tools write files in place. If two workspaces, or a
//! workspace and the object store, shared one file on disk, an in-place write
//! in one workspace would silently change another agent's files or corrupt
//! the stored object every later workspace is made from.

use std::fs::{self, OpenOptions};
use std::io::Write;

use rosen_client::workspace::{materialise, Options, SourceWorkspace};
use rosen_core::{FileMode, Tree, TreeEntry};
use rosen_store::{LooseObjectStore, ObjectStore};

#[test]
fn an_in_place_write_reaches_no_other_workspace_or_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = LooseObjectStore::open(dir.path().join("objects")).unwrap();
    let original = b"fn main() {}\n";
    let blob = store.put_blob(original).unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "main.rs", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();

    let first = dir.path().join("first");
    materialise(&store, tree, &first, &Options::default()).unwrap();
    // The second workspace takes the first as its source, so on a volume that
    // block clones it exercises the clone path, and elsewhere the fallback.
    let second = dir.path().join("second");
    let from_first = Options {
        source: Some(SourceWorkspace {
            dir: first.clone(),
            tree,
        }),
        ..Options::default()
    };
    materialise(&store, tree, &second, &from_first).unwrap();

    // Overwrite the first workspace's file in place, without truncating or
    // replacing it, as an editor or formatter may.
    let mut file = OpenOptions::new()
        .write(true)
        .open(first.join("main.rs"))
        .unwrap();
    file.write_all(b"EDITED").unwrap();
    drop(file);

    assert_eq!(fs::read(second.join("main.rs")).unwrap(), original);
    assert_eq!(store.read_blob(&blob).unwrap().data, original);
    let third = dir.path().join("third");
    materialise(&store, tree, &third, &Options::default()).unwrap();
    assert_eq!(fs::read(third.join("main.rs")).unwrap(), original);
}
