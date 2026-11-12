//! Differential check of the loose-object backend against real git.
//!
//! Risk: SPEC D5 and R1 rely on the store's files being exactly git's loose
//! objects. A wrong header, compression framing or path layout would make
//! imported history unreadable and pushes to the mirror fail or corrupt.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use rosen_core::{Commit, FileMode, ObjectId, Signature, Tree, TreeEntry};
use rosen_store::{LooseObjectStore, ObjectStore};

fn git(repo: &Path, args: &[&str], stdin: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_DATE", "1700000000 -0130")
        .env("GIT_COMMITTER_DATE", "1700000000 -0130")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git is installed");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "git {args:?}: {stderr}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn loose_objects_are_interchangeable_with_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "--bare", "--quiet"], b"");
    let store = LooseObjectStore::open(repo.join("objects")).unwrap();

    // Rosen writes; git must find the same IDs and a sound repository.
    let blob = store.put_blob(b"hello\r\nworld\n").unwrap();
    let file = TreeEntry::new(FileMode::Regular, "a.txt", blob).unwrap();
    let sub = store
        .put_tree(&Tree::new(vec![file.clone()]).unwrap())
        .unwrap();
    let dir_entry = TreeEntry::new(FileMode::Directory, "a", sub).unwrap();
    let root = store
        .put_tree(&Tree::new(vec![dir_entry, file]).unwrap())
        .unwrap();
    let who = Signature::new("Agent One", "agent@rosen.invalid", 1_700_000_000);
    let first = store
        .put_commit(&Commit {
            tree: root,
            parents: vec![],
            author: who.clone(),
            committer: who,
            extra_headers: vec![],
            message: b"first\n".to_vec(),
        })
        .unwrap();
    let hashed = git(repo, &["hash-object", "--stdin"], b"hello\r\nworld\n");
    assert_eq!(ObjectId::from_hex(hashed.trim()).unwrap(), blob);
    git(repo, &["fsck", "--strict", "--no-dangling"], b"");

    // Git writes; the store must read and verify it.
    let args = ["-c", "user.name=G", "-c", "user.email=g@rosen.invalid"];
    let tree = root.to_hex();
    let parent = first.to_hex();
    let commit_args = [&args[..], &["commit-tree", &tree, "-p", &parent]].concat();
    let second = ObjectId::from_hex(git(repo, &commit_args, b"second\n").trim()).unwrap();
    let read = store.read_commit(&second).unwrap();
    assert_eq!((read.tree, read.parents), (root, vec![first]));
    assert!(store.contains(&second).unwrap());
    assert!(store.read_tree(&second).is_err(), "kind is checked");
}
