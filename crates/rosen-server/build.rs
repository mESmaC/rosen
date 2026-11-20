//! Records the git commit this server is built from, which `status` reports
//! so a client can tell one build from another (#425).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Names the commit for a build whose source tree is not a git checkout,
/// such as a Rosen workspace or an unpacked archive. It wins over git.
const COMMIT_ENV: &str = "ROSEN_BUILD_COMMIT";

/// This crate's directory in the repository. A checkout that holds it
/// anywhere else is some other project's, whose commit says nothing here.
const CRATE_PREFIX: &str = "crates/rosen-server/";

fn main() {
    println!("cargo:rerun-if-env-changed={COMMIT_ENV}");
    let named = std::env::var(COMMIT_ENV)
        .ok()
        .map(|commit| commit.trim().to_owned())
        .filter(|commit| is_commit_name(commit));
    // Left unset when unknown: the server then reports no commit.
    if let Some(commit) = named.or_else(checkout_commit) {
        println!("cargo:rustc-env=ROSEN_SERVER_BUILD_COMMIT={commit}");
    }
}

/// One printable word, so it can neither end the `cargo:` line it is
/// written on nor start another.
fn is_commit_name(commit: &str) -> bool {
    !commit.is_empty() && commit.bytes().all(|byte| byte.is_ascii_graphic())
}

/// The checked-out commit of the git repository this crate is built in, if
/// it is in Rosen's own and `git` runs.
fn checkout_commit() -> Option<String> {
    let source = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    if git(&source, &["rev-parse", "--show-prefix"])? != CRATE_PREFIX {
        return None;
    }
    let commit = git(&source, &["rev-parse", "HEAD"])?;
    if !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    // `HEAD` changes when another branch is checked out, and its log gains a
    // line whenever the checked-out commit moves, as a commit or a pull does.
    for file in ["HEAD", "logs/HEAD"] {
        let path = git(&source, &["rev-parse", "--git-path", file]).map(|path| source.join(path));
        // A path that does not exist would rerun this script on every build.
        if let Some(path) = path.filter(|path| path.is_file()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    Some(commit)
}

/// One git command's trimmed output, or `None` when git is missing, fails
/// (outside a repository, say) or prints nothing.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}
