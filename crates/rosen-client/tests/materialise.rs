//! SPEC.md R3: hardlinks are never used for files in a workspace.
//!
//! Risk: agents' tools write files in place. If two workspaces, or a
//! workspace and the object store, shared one file on disk, an in-place write
//! in one workspace would silently change another agent's files or corrupt
//! the stored object every later workspace is made from.

use std::fs::{self, OpenOptions};
use std::io::Write;

use rosen_client::workspace::{materialise, Method, Options, SourceWorkspace};
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

/// SPEC.md R3, issue #39: the block clone path on a real ReFS volume.
///
/// Risk: CI's NTFS runners and most machines never take the clone path, so
/// a clone that fails or returns wrong bytes there would ship unnoticed and
/// every warm pool workspace would silently fall back to a full copy. The
/// `windows-refs` CI job puts the temp directory on a Dev Drive and sets
/// `ROSEN_REQUIRE_BLOCK_CLONE`, so there the fallback fails this test.
#[test]
fn a_block_clone_is_brought_to_the_new_tree_and_keeps_build_output() {
    let dir = tempfile::tempdir().unwrap();
    let store = LooseObjectStore::open(dir.path().join("objects")).unwrap();
    let before = put_tree(
        &store,
        &[
            ("keep.rs", b"keep"),
            ("edit.rs", b"old"),
            ("gone.rs", b"gone"),
            ("becomes_dir", b"a file"),
            ("emptied/only.rs", b"only"),
        ],
    );
    let after_files: &[(&str, &[u8])] = &[
        ("keep.rs", b"keep"),
        ("edit.rs", b"new"),
        ("added.rs", b"added"),
        ("becomes_dir/inner.rs", b"inner"),
        ("fresh", b"fresh"),
        ("newdir/f.rs", b"f"),
    ];
    let after = put_tree(&store, after_files);

    let source = dir.path().join("source");
    materialise(&store, before, &source, &Options::default()).unwrap();
    fs::create_dir(source.join("target")).unwrap();
    fs::write(source.join("target/out.bin"), b"build output").unwrap();
    // Risk: a clone stamped with the time it was made looks newer than the
    // build output that depends on it, and cargo rebuilds every crate in a
    // prepared workspace's first build. Clones keep their sources' times.
    let built_at =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for old in ["keep.rs", "target/out.bin"] {
        let file = OpenOptions::new()
            .write(true)
            .open(source.join(old))
            .unwrap();
        file.set_modified(built_at).unwrap();
    }
    // Untracked output where the new tree tracks a path (issue #40): a file
    // over a file, a directory over a file, a file over a directory.
    fs::write(source.join("added.rs"), b"stale generated").unwrap();
    fs::create_dir(source.join("fresh")).unwrap();
    fs::write(source.join("fresh/x.o"), b"object").unwrap();
    fs::write(source.join("newdir"), b"artefact").unwrap();

    let dest = dir.path().join("dest");
    let options = Options {
        source: Some(SourceWorkspace {
            dir: source,
            tree: before,
        }),
        ..Options::default()
    };
    let report = materialise(&store, after, &dest, &options).unwrap();
    for (path, data) in after_files {
        assert_eq!(fs::read(dest.join(path)).unwrap(), *data, "{path}");
    }
    if report.method != Method::BlockClone {
        assert!(
            std::env::var_os("ROSEN_REQUIRE_BLOCK_CLONE").is_none(),
            "block clone required, got {report:?}"
        );
        return;
    }
    // Five tracked files and four untracked, all cloned, none copied.
    assert_eq!((report.cloned, report.copied, report.written), (9, 0, 5));
    assert_eq!(
        fs::read(dest.join("target/out.bin")).unwrap(),
        b"build output"
    );
    assert!(!dest.join("gone.rs").exists());
    assert!(!dest.join("emptied").exists());
    for kept in ["keep.rs", "target/out.bin"] {
        let modified = fs::metadata(dest.join(kept)).unwrap().modified().unwrap();
        assert_eq!(modified, built_at, "{kept}");
    }
}

/// Stores a tree of regular files given as `/`-separated paths, one level of
/// directories deep.
fn put_tree(store: &LooseObjectStore, files: &[(&str, &[u8])]) -> rosen_core::ObjectId {
    let mut entries = Vec::new();
    let mut dirs: std::collections::BTreeMap<&str, Vec<TreeEntry>> = Default::default();
    for (path, data) in files {
        let blob = store.put_blob(data).unwrap();
        match path.split_once('/') {
            Some((dir, name)) => dirs
                .entry(dir)
                .or_default()
                .push(TreeEntry::new(FileMode::Regular, name, blob).unwrap()),
            None => entries.push(TreeEntry::new(FileMode::Regular, *path, blob).unwrap()),
        }
    }
    for (dir, children) in dirs {
        let id = store.put_tree(&Tree::new(children).unwrap()).unwrap();
        entries.push(TreeEntry::new(FileMode::Directory, dir, id).unwrap());
    }
    store.put_tree(&Tree::new(entries).unwrap()).unwrap()
}
