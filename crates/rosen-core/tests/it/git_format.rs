//! Risk: import and the mirror push (R1, D5) rely on Rosen hashing objects
//! exactly as git does. A one-byte encoding drift would give every imported
//! commit a new ID and make the mirror push a non-fast-forward. The expected
//! IDs below were produced by `git hash-object` / `git commit`.

use rosen_core::history::LandedTrailers;
use rosen_core::{Blob, FileMode, Object, ObjectId, Tree, TreeEntry};

fn id(hex: &str) -> ObjectId {
    hex.parse().unwrap()
}

#[test]
fn objects_hash_and_round_trip_like_git() {
    let blob = Object::from(Blob {
        data: b"hello\n".to_vec(),
    });
    assert_eq!(blob.id(), id("ce013625030ba8dba906f756967f9e9ca394464a"));
    assert_eq!(
        Object::from(Tree::default()).id(),
        id("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
    );

    // Git orders the directory `src` as `src/`, after the file `src-c`.
    let any = blob.id();
    let entries = vec![
        TreeEntry::new(
            FileMode::Directory,
            "src",
            id("4b825dc642cb6eb9a060e54bf8d69288fbee4904"),
        )
        .unwrap(),
        TreeEntry::new(FileMode::Regular, "src-c", any).unwrap(),
    ];
    let tree = Tree::new(entries).unwrap();
    assert_eq!(tree.entries()[0].name, b"src-c");

    let commit_body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author Z\xc3\xb6 <z@e> 1700000000 +0100\n\
committer rosen <server@rosen> 1700000001 -0730\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\
\nLand\n\nRosen-Change: zyxwvutsrqponmlkzyxwvutsrqponmlk\nRosen-Approved-By: duty\n";
    let raw = [
        format!("commit {}\0", commit_body.len()).as_bytes(),
        commit_body,
    ]
    .concat();
    let commit = Object::decode_loose(&raw).unwrap();
    assert_eq!(commit.encode_loose(), raw);
    assert_eq!(commit.id(), ObjectId::hash_object("commit", commit_body));
    let Object::Commit(commit) = commit else {
        panic!("not a commit")
    };
    let trailers = LandedTrailers::parse(&commit.message).unwrap();
    assert_eq!(
        trailers.change.as_bytes(),
        &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef].repeat(2)[..]
    );
    assert_eq!(trailers.receipt, None);
}
