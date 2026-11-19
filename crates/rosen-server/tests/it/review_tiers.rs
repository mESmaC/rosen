//! Risk (R7): removing sensitive policy must not lower a change's own tier.
use rosen_core::{Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_server::{landing::Tier, reviews::tiers::compute};
use rosen_store::{MemoryObjectStore, ObjectStore};

fn tree(store: &MemoryObjectStore, files: &[(&str, FileMode, &[u8])]) -> ObjectId {
    let entries = files
        .iter()
        .map(|(name, mode, data)| {
            TreeEntry::new(*mode, *name, store.put_blob(data).unwrap()).unwrap()
        })
        .collect();
    store.put_tree(&Tree::new(entries).unwrap()).unwrap()
}

fn commit(store: &MemoryObjectStore, tree: ObjectId, parent: Option<ObjectId>) -> ObjectId {
    let who = Signature::new("test", "test@rosen", 0);
    store
        .put_commit(&Commit {
            tree,
            parents: parent.into_iter().collect(),
            author: who.clone(),
            committer: who,
            message: b"tier\n".to_vec(),
            extra_headers: vec![],
        })
        .unwrap()
}

#[test]
fn tier_uses_base_policy_and_both_sides_of_the_diff() {
    use FileMode::{Executable, Regular, Symlink};
    let store = MemoryObjectStore::new();
    let policy = b"sensitive=['protected.md']\nworkspace_inputs=['input.txt']\n";
    let base_tree = tree(&store, &[("rosen.toml", Regular, policy)]);
    let base = commit(&store, base_tree, None);
    let classify = |files: &[(&str, FileMode, &[u8])]| {
        compute(&store, &base, &base, &tree(&store, files)).unwrap()
    };
    assert_eq!(
        classify(&[
            ("rosen.toml", Regular, policy),
            ("notes.md", Regular, b"prose")
        ]),
        Tier::ProseOnly
    );
    assert_eq!(
        classify(&[
            ("rosen.toml", Regular, b"sensitive=[]"),
            ("protected.md", Regular, b"prose")
        ]),
        Tier::Sensitive
    );
    for (name, mode, bytes) in [
        ("source.rs", Regular, b"// comment".as_slice()),
        ("input.txt", Regular, b"input"),
        ("notes.md", Executable, b"text"),
        ("notes.md", Symlink, b"target"),
        ("notes.md", Regular, b"binary\0"),
        ("notes.md", Regular, b"\xff"),
    ] {
        assert_eq!(
            classify(&[("rosen.toml", Regular, policy), (name, mode, bytes)]),
            Tier::Normal,
            "{name}: {mode:?}"
        );
    }
    check_removed_paths(&store, base);
}

fn check_removed_paths(store: &MemoryObjectStore, base: ObjectId) {
    use FileMode::Regular;
    let protected = tree(store, &[("protected.md", Regular, b"old")]);
    let renamed = tree(store, &[("notes.md", Regular, b"old")]);
    assert_eq!(
        compute(store, &base, &protected, &renamed).unwrap(),
        Tier::Sensitive
    );
    let empty = store.put_tree(&Tree::default()).unwrap();
    assert_eq!(
        compute(store, &base, &renamed, &empty).unwrap(),
        Tier::ProseOnly
    );
}
