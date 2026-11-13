use rosen_core::{AgentId, Change, ChangeId, ChangeParent, FileMode, Tree, TreeEntry};
use rosen_merge::FileEdit;
use rosen_server::overlaps::edits::edited_files;
use rosen_store::{LooseObjectStore, ObjectStore};
use std::collections::HashMap;

// SPEC R6 risk: treating mode/binary/absent paths as line edits hides overlaps.
#[test]
fn region_classification_matches_proven_fixtures() {
    let temp = tempfile::tempdir().unwrap();
    let store = LooseObjectStore::open(temp.path()).unwrap();
    let parent = ChangeId::from_bytes([1; 16]);
    let rows = include_str!("../../../proofs/fixtures/live-edits.tsv");
    for row in rows.lines().skip(1) {
        let bits: Vec<bool> = row.split('\t').map(|v| v == "true").collect();
        let [p, h, same, regular, binary, whole] = bits[..] else {
            panic!("{row}")
        };
        // No path exists if absent on both sides: it has no regions at all.
        if !p && !h {
            continue;
        }
        let mode = if regular {
            FileMode::Regular
        } else {
            FileMode::Symlink
        };
        let old = tree(&store, p, mode, b"old\n");
        let mode = if same { mode } else { FileMode::Executable };
        let new = tree(&store, h, mode, if binary { b"new\0\n" } else { b"new\n" });
        let change = Change::new(
            ChangeId::from_bytes([2; 16]),
            AgentId::new("test").unwrap(),
            ChangeParent::Change(parent),
            new,
        );
        let files = edited_files(&store, &HashMap::from([(parent, old)]), &change).unwrap();
        assert_eq!(files["file"].regions == FileEdit::Whole, whole, "{row}");
    }
}

fn tree(
    store: &dyn ObjectStore,
    present: bool,
    mode: FileMode,
    bytes: &[u8],
) -> rosen_core::ObjectId {
    let entries = if present {
        vec![TreeEntry::new(mode, "file", store.put_blob(bytes).unwrap()).unwrap()]
    } else {
        vec![]
    };
    store.put_tree(&Tree::new(entries).unwrap()).unwrap()
}
