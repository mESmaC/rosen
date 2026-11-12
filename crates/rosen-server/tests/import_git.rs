//! End-to-end check of `import` (SPEC R1) against real git.
//!
//! Risk: import is the only way history enters Rosen. If an object is
//! re-encoded with a new id, every later mirror push stops being a
//! fast-forward; if the base can be set over an existing one, an import
//! moves the base without a land.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use rosen_protocol::{Endpoint, ErrorCode, Outcome, Response};
use rosen_server::{import::service, ServerConfig, ServerState};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .expect("git is installed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "git {args:?}: {stderr}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A source with a merge, a nested tree, an executable and a file-mode-only
/// change, on a branch named `trunk` so the default is read from HEAD.
fn make_source(dir: &Path) -> String {
    git(dir, &["init", "--quiet", "--initial-branch=trunk"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    std::fs::create_dir_all(dir.join("src/nested")).unwrap();
    std::fs::write(dir.join("src/nested/a.txt"), "one\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "--quiet", "-m", "first"]);
    git(dir, &["checkout", "--quiet", "-b", "side"]);
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["update-index", "--chmod=+x", "run.sh"]);
    git(dir, &["commit", "--quiet", "-m", "side"]);
    git(dir, &["checkout", "--quiet", "trunk"]);
    std::fs::write(dir.join("src/nested/a.txt"), "one\ntwo\n").unwrap();
    git(dir, &["commit", "--quiet", "-am", "second"]);
    git(
        dir,
        &["merge", "--quiet", "--no-ff", "-m", "merge side", "side"],
    );
    git(dir, &["rev-parse", "HEAD"])
}

fn import(state: &Arc<ServerState>, source: &Path) -> Outcome {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let source = source.to_string_lossy().into_owned();
    runtime.block_on(service::handle(state, source, None))
}

#[test]
fn import_keeps_git_ids_and_never_replaces_the_base() {
    let source = tempfile::tempdir().unwrap();
    let tip = make_source(source.path());
    let data = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.path().to_path_buf(),
        endpoint: Endpoint::new("rosen-import-test-unused"),
    });

    let Outcome::Ok(Response::Imported(summary)) = import(&state, source.path()) else {
        panic!("first import failed");
    };
    assert_eq!(
        (summary.branch.as_str(), summary.base_commit.as_str()),
        ("trunk", tip.as_str())
    );

    // Git reading Rosen's store as its object directory must find the whole
    // history under the source's own ids.
    let checker = tempfile::tempdir().unwrap();
    git(checker.path(), &["init", "--bare", "--quiet"]);
    let alternates = checker.path().join("objects/info/alternates");
    std::fs::write(
        alternates,
        data.path().join("objects").to_string_lossy().as_bytes(),
    )
    .unwrap();
    git(
        checker.path(),
        &["fsck", "--connectivity-only", "--no-dangling", &tip],
    );
    let expected = git(source.path(), &["log", "--format=%H %T %P", &tip]);
    assert_eq!(
        git(checker.path(), &["log", "--format=%H %T %P", &tip]),
        expected
    );

    let Outcome::Error(refused) = import(&state, source.path()) else {
        panic!("a second import replaced the base");
    };
    assert_eq!(refused.code, ErrorCode::BaseAlreadySet);
}
