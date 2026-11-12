//! One push of the base to the mirror remote, run by the `git` binary.
//!
//! The store's files are git's loose objects (SPEC D5), so git reads them in
//! place: a small bare staging repository lists the store in its
//! `objects/info/alternates`. Git treats an alternate as read-only, so it
//! never writes into, repacks or prunes the authoritative store.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rosen_core::ObjectId;

use crate::import::git_command;

/// A push still running after this long is stopped and retried, so a hung
/// connection cannot stall the mirror forever. Generous, because the first
/// push sends the whole imported history.
const PUSH_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Why a push did not land on the remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushFailure {
    /// The remote refused the update, for example because its branch has
    /// commits the base does not (it is not a fast-forward). Retrying alone
    /// will not fix it.
    pub rejected: bool,
    /// What git reported, with any credentials in the remote URL hidden.
    pub message: String,
}

/// Pushes `commit` to `refs/heads/<branch>` on `remote`.
///
/// The refspec has no `+` and git gets no `--force`, so git itself refuses
/// any update that is not a fast-forward (SPEC R1): the mirror never loses
/// a commit, even one pushed there by something other than Rosen.
pub async fn push(
    staging: &Path,
    objects_dir: &Path,
    remote: &str,
    branch: &str,
    commit: &ObjectId,
) -> Result<(), PushFailure> {
    let fail = |message: String| PushFailure {
        rejected: false,
        message: hide_credentials(remote, &message),
    };
    // Absolute, because git runs with `-C` and would read a relative path
    // against that directory instead.
    let staging = &std::path::absolute(staging)
        .map_err(|error| fail(format!("staging path {}: {error}", staging.display())))?;
    prepare_staging(staging, objects_dir)
        .await
        .map_err(|error| fail(format!("preparing {}: {error}", staging.display())))?;
    let refspec = format!("{}:refs/heads/{branch}", commit.to_hex());
    let mut command = tokio::process::Command::from(git_command(staging));
    // `--` keeps a remote that starts with `-` from being read as an option.
    command
        .args(["push", "--porcelain", "--", remote, refspec.as_str()])
        // A missing credential must fail the push, never wait on a prompt
        // that no one at a headless server will answer.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|error| fail(format!("could not run git: {error}")))?;
    let output = tokio::time::timeout(PUSH_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| fail(format!("git push did not finish within {PUSH_TIMEOUT:?}")))?
        .map_err(|error| fail(format!("git push: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(PushFailure {
        // Porcelain output marks a refused ref with `!` and `[rejected]` or
        // `[remote rejected]`.
        rejected: stdout.lines().any(|line| line.starts_with('!')),
        message: hide_credentials(remote, &format!("{}\n{}", stdout.trim(), stderr.trim())),
    })
}

/// Creates the staging repository on first use, and points its alternates
/// at the store on every push, so a moved data directory still works.
async fn prepare_staging(staging: &Path, objects_dir: &Path) -> std::io::Result<()> {
    if !staging.join("HEAD").is_file() {
        let parent = staging.parent().unwrap_or(staging);
        std::fs::create_dir_all(parent)?;
        let mut init = tokio::process::Command::from(git_command(parent));
        init.args(["init", "--bare", "--quiet"]).arg(staging);
        run_quietly(init).await?;
        let mut config = tokio::process::Command::from(git_command(staging));
        // The staging repository holds no objects of its own; this stops
        // git from ever starting maintenance in it.
        config.args(["config", "gc.auto", "0"]);
        run_quietly(config).await?;
    }
    let objects = std::path::absolute(objects_dir)?;
    // Git reads alternates with forward slashes on every platform.
    let line = format!("{}\n", objects.to_string_lossy().replace('\\', "/"));
    let info = staging.join("objects").join("info");
    std::fs::create_dir_all(&info)?;
    std::fs::write(info.join("alternates"), line)
}

async fn run_quietly(mut command: tokio::process::Command) -> std::io::Result<()> {
    let output = command.output().await?;
    if output.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

/// Replaces the remote's `user:token@` part in `text`, so a token in a
/// remote URL never reaches an event that every subscriber can read.
pub fn hide_credentials(remote: &str, text: &str) -> String {
    let Some((scheme, rest)) = remote.split_once("://") else {
        return text.to_owned();
    };
    let host_start = rest.find('/').unwrap_or(rest.len());
    match rest[..host_start].rfind('@') {
        Some(at) if at > 0 => {
            let safe = format!("{scheme}://***@{}", &rest[at + 1..]);
            text.replace(remote, &safe).replace(&rest[..at], "***")
        }
        _ => text.to_owned(),
    }
}
