//! Proactive HTTP basic auth fallback for HTTPS mirror reads (SPEC R1, #432).
//!
//! Private hosts such as Collapse and GitHub answer an unauthenticated HTTPS
//! read with 404 rather than 401 so they do not reveal that a private
//! repository exists. Without proactive auth, git never consults the
//! credential helper and the read fails with "repository not found".

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Minimum git `(major, minor)` version that supports `http.proactiveAuth`.
const MIN_GIT_VERSION: (u32, u32) = (2, 46);

/// Git flags enabling proactive basic auth for one command invocation.
pub(super) const PROACTIVE_AUTH_ARGS: [&str; 2] = ["-c", "http.proactiveAuth=basic"];

/// Reported when an HTTPS mirror read fails with not-found on a git older than 2.46.
pub const OLD_GIT_MESSAGE: &str =
    "mirror read got not-found. Upgrade git to 2.46 or later, or set http.<url>.proactiveAuth";

/// Per-server memory of whether HTTPS mirror reads needed proactive basic auth.
#[derive(Default, Debug)]
pub struct ProactiveAuth {
    enabled: AtomicBool,
}

impl ProactiveAuth {
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }
}

/// True only for `https://` remotes. Plaintext `http://` never sends
/// credentials proactively without a challenge.
pub(super) fn is_https(remote: &str) -> bool {
    remote
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

/// True when the git subcommand reads from the remote rather than the local
/// staging repository.
pub(super) fn is_remote_read(args: &[&str]) -> bool {
    matches!(args.first().copied(), Some("ls-remote" | "fetch"))
}

/// True when git's stderr reports a missing repository (HTTP 404).
///
/// Matches both host-supplied `remote: Repository not found.` messages and
/// git's own `fatal: repository '<url>' not found` diagnostic.
pub(super) fn is_not_found(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("repository not found") {
        return true;
    }
    lower.lines().any(|line| {
        line.split_once("repository '")
            .is_some_and(|(_, rest)| rest.contains("' not found"))
    })
}

/// Parses `(major, minor)` from `git --version` output such as
/// `git version 2.47.0.windows.1` or `git version 2.39.5 (Apple Git-154)`.
pub(super) fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    let rest = output.trim().strip_prefix("git version ")?;
    let version = rest.split_whitespace().next()?;
    let mut parts = version.split('.');
    let major = leading_digits(parts.next()?)?;
    let minor = leading_digits(parts.next()?)?;
    Some((major, minor))
}

fn leading_digits(part: &str) -> Option<u32> {
    let end = part
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(part.len());
    part[..end].parse().ok()
}

pub(super) fn version_supports_proactive_auth(version: (u32, u32)) -> bool {
    version >= MIN_GIT_VERSION
}

/// True when the installed `git` binary is 2.46 or later.
pub(super) async fn git_supports_proactive_auth() -> bool {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("--version")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(10), command.output()).await
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_git_version(&text).is_some_and(version_supports_proactive_auth)
}
