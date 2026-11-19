//! Risk: the configuration decides review tiers and the check set every
//! receipt must cover (SPEC R7, R8). If an invalid `rosen.toml` on the base
//! took effect as the defaults, one bad edit would empty the sensitive list
//! and rename the check set; if it were refused outright, the project could
//! not land the fix. The base must keep its last valid configuration.

use rosen_core::{Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_server::project_config::resolve;
use rosen_store::{MemoryObjectStore, ObjectStore};

fn commit(store: &MemoryObjectStore, config: &str, parent: Option<ObjectId>) -> ObjectId {
    let blob = store.put_blob(config.as_bytes()).unwrap();
    let entry = TreeEntry::new(FileMode::Regular, "rosen.toml", blob).unwrap();
    let tree = store.put_tree(&Tree::new(vec![entry]).unwrap()).unwrap();
    let who = Signature::new("t", "t@t", 0);
    store
        .put_commit(&Commit {
            tree,
            parents: parent.into_iter().collect(),
            author: who.clone(),
            committer: who,
            extra_headers: Vec::new(),
            message: b"c\n".to_vec(),
        })
        .unwrap()
}

#[test]
fn an_invalid_file_on_the_base_keeps_the_last_valid_configuration() {
    let store = MemoryObjectStore::new();
    let valid = "sensitive = [\"crates/rosen-store/\"]\n[checks.pre_land]\nname = \"build\"\n";
    let good = commit(&store, valid, None);
    // A misspelt key, then a check set sharing the post-land name.
    let typo = commit(&store, "sensitiv = []\n", Some(good));
    let clash = "[checks.pre_land]\nname = \"post-land\"\n";
    let tip = commit(&store, clash, Some(typo));

    let resolved = resolve(&store, &tip).unwrap();
    assert_eq!(resolved.source, Some(good));
    assert!(resolved.error.is_some_and(|e| e.contains("post-land")));
    assert_eq!(resolved.config.checks.pre_land.name, "build");
    assert!(resolved
        .config
        .is_sensitive("crates/rosen-store/src/loose.rs"));

    let fixed = commit(&store, "", Some(tip));
    let resolved = resolve(&store, &fixed).unwrap();
    assert_eq!((resolved.source, resolved.error), (Some(fixed), None));
    assert_eq!(resolved.config.checks.pre_land.name, "pre-land");
}
