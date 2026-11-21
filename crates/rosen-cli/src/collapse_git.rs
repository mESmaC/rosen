//! Git credential and proactive-auth helpers for `rosen collapse link` and
//! `rosen collapse unlink` (SPEC R1, R10, #437).
//!
//! Push tokens are read only from stdin or `ROSEN_COLLAPSE_TOKEN`, never from
//! command-line arguments, and reach `git credential approve` on its stdin so
//! another process listing arguments never sees them.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rosen_client::Error;
use tokio::io::AsyncWriteExt;

/// Environment variable providing the Collapse push token when stdin is not piped.
pub const TOKEN_ENV: &str = "ROSEN_COLLAPSE_TOKEN";

/// Timeout for one local `git credential` or `git config` call.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Git repository override variables removed so a random enclosing repo never
/// redirects global credential or config calls.
const REPOSITORY_OVERRIDES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

/// Reads the Collapse push token from `ROSEN_COLLAPSE_TOKEN` or piped stdin.
/// Checking the environment variable first prevents hanging in non-TTY runners
/// whose stdin is an open pipe. Never prompts on a terminal and never echoes
/// the token in an error message.
pub fn read_push_token() -> Result<String, Error> {
    if let Ok(raw) = std::env::var(TOKEN_ENV) {
        let token = raw.trim_end_matches(['\r', '\n']);
        if !token.is_empty() {
            return validate_token(token);
        }
    }
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut raw = String::new();
        stdin.lock().read_to_string(&mut raw)?;
        let token = raw.trim_end_matches(['\r', '\n']);
        if !token.is_empty() {
            return validate_token(token);
        }
    }
    Err(Error::InvalidArgument(format!(
        "no push token: pipe the token on stdin or set {TOKEN_ENV}"
    )))
}

fn validate_token(token: &str) -> Result<String, Error> {
    if token.chars().any(char::is_control) {
        return Err(Error::InvalidArgument(
            "push token must be a single line with no control characters".to_owned(),
        ));
    }
    Ok(token.to_owned())
}

/// Directory to run git commands from: matches `rosen-server`'s `collapse_check`
/// directory when the server's data directory is local.
pub fn existing_git_dir(data_dir: &str) -> PathBuf {
    let path = Path::new(data_dir);
    if path.is_dir() {
        path.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

/// Stores `token` in git's credential helper for every HTTPS URL in `urls`.
pub async fn approve_credentials(
    git_dir: &Path,
    urls: &[&str],
    token: &str,
) -> Result<Vec<String>, Error> {
    let mut approved = Vec::with_capacity(urls.len());
    for &url in urls {
        if !is_https(url) {
            continue;
        }
        let body = format!("url={url}\nusername=rosen\npassword={token}\n\n");
        run_git_stdin(git_dir, &["credential", "approve"], &body, Some(token)).await?;
        approved.push(url.to_owned());
    }
    Ok(approved)
}

/// Removes stored path-scoped credentials from git's credential helper for
/// every HTTPS URL that carries a non-empty path.
///
/// Forces `credential.useHttpPath = true` for the reject call so unlinking
/// never removes a host-wide credential or another repository's entries even
/// when global git config lacks `useHttpPath` (SPEC R1, #442).
pub async fn reject_credentials(git_dir: &Path, urls: &[&str]) -> Result<Vec<String>, Error> {
    let mut forgotten = Vec::with_capacity(urls.len());
    for &url in urls {
        if !has_https_path(url) {
            continue;
        }
        let body = format!("url={url}\n\n");
        let args = reject_args(url);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git_stdin(git_dir, &arg_refs, &body, None).await?;
        forgotten.push(url.to_owned());
    }
    Ok(forgotten)
}

/// Configures `credential.<origin>.useHttpPath = true` in global git config
/// before approving credentials so git keys Collapse tokens by full URL path
/// instead of overwriting one host-wide entry (SPEC R1, #442).
pub async fn set_use_http_path(git_dir: &Path, origin: &str) -> Result<bool, Error> {
    if !is_https(origin) {
        return Ok(false);
    }
    let key = use_http_path_key(origin);
    run_git_config(git_dir, &["config", "--global", &key, "true"]).await?;
    Ok(true)
}

/// Configures `http.<mirror_url>.proactiveAuth = basic` in global git config so
/// private HTTPS mirror reads send credentials on the first request (SPEC R1).
pub async fn set_proactive_auth(git_dir: &Path, mirror_url: &str) -> Result<bool, Error> {
    if !is_https(mirror_url) {
        return Ok(false);
    }
    let key = proactive_auth_key(mirror_url);
    run_git_config(git_dir, &["config", "--global", &key, "basic"]).await?;
    Ok(true)
}

/// Clears `http.<mirror_url>.proactiveAuth` from global git config when present.
pub async fn unset_proactive_auth(git_dir: &Path, mirror_url: &str) -> Result<bool, Error> {
    if !is_https(mirror_url) {
        return Ok(false);
    }
    let key = proactive_auth_key(mirror_url);
    let mut command = git_command(git_dir);
    command
        .args(["config", "--global", "--unset-all", &key])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_with_timeout(command, "git config --unset-all").await?;
    match output.status.code() {
        Some(0) => Ok(true),
        // `git config --unset-all` exits 5 when the key was not present.
        Some(5) => Ok(false),
        _ => Err(git_error("git config --unset-all", &output, None)),
    }
}

fn use_http_path_key(origin: &str) -> String {
    let origin = origin.trim_end_matches('/');
    format!("credential.{origin}.useHttpPath")
}

fn proactive_auth_key(mirror_url: &str) -> String {
    format!("http.{mirror_url}.proactiveAuth")
}

fn https_origin(url: &str) -> Option<String> {
    if !is_https(url) {
        return None;
    }
    let rest = &url[8..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    (!host_port.is_empty()).then(|| format!("https://{host_port}"))
}

fn has_https_path(url: &str) -> bool {
    if !is_https(url) {
        return false;
    }
    let Some((_, path)) = url[8..].split_once('/') else {
        return false;
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    !path.trim_matches('/').is_empty()
}

fn reject_args(url: &str) -> Vec<String> {
    let mut args = vec!["-c".to_owned(), "credential.useHttpPath=true".to_owned()];
    if let Some(origin) = https_origin(url) {
        if !origin.contains('=') {
            args.push("-c".to_owned());
            args.push(format!("credential.{origin}.useHttpPath=true"));
        }
    }
    if !url.contains('=') {
        args.push("-c".to_owned());
        args.push(format!("credential.{url}.useHttpPath=true"));
    }
    args.push("credential".to_owned());
    args.push("reject".to_owned());
    args
}

pub fn is_https(url: &str) -> bool {
    url.get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

fn git_command(directory: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(directory);
    for name in REPOSITORY_OVERRIDES {
        command.env_remove(name);
    }
    if let Some(parent) = directory.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("LC_ALL", "C")
        .kill_on_drop(true);
    command
}

async fn run_git_config(git_dir: &Path, args: &[&str]) -> Result<(), Error> {
    let mut command = git_command(git_dir);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_with_timeout(command, "git config").await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_error("git config", &output, None))
    }
}

async fn run_git_stdin(
    git_dir: &Path,
    args: &[&str],
    input: &str,
    redact: Option<&str>,
) -> Result<(), Error> {
    let label = format!("git {}", args.join(" "));
    let mut command = git_command(git_dir);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|err| {
        Error::Io(std::io::Error::other(format!(
            "could not run `{label}`: {err}"
        )))
    })?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin.write_all(input.as_bytes()).await?;
    drop(stdin);
    let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| Error::Io(std::io::Error::other(format!("`{label}` timed out"))))??;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_error(&label, &output, redact))
    }
}

async fn run_with_timeout(
    mut command: tokio::process::Command,
    label: &str,
) -> Result<std::process::Output, Error> {
    let child = command.spawn().map_err(|err| {
        Error::Io(std::io::Error::other(format!(
            "could not run `{label}`: {err}"
        )))
    })?;
    tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| Error::Io(std::io::Error::other(format!("`{label}` timed out"))))?
        .map_err(Error::Io)
}

fn git_error(label: &str, output: &std::process::Output, redact: Option<&str>) -> Error {
    let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if let Some(secret) = redact.filter(|s| !s.is_empty()) {
        stderr = stderr.replace(secret, "***");
    }
    let detail = if stderr.is_empty() {
        format!("`{label}` exited with {}", output.status)
    } else {
        format!("`{label}` failed: {stderr}")
    };
    Error::Io(std::io::Error::other(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validation_rejects_control_characters() {
        assert_eq!(validate_token("tok_abc123").unwrap(), "tok_abc123");
        assert!(validate_token("line1\nline2").is_err());
        assert!(validate_token("tok\r").is_err());
    }

    #[test]
    fn git_error_redacts_secret() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: b"helper rejected SECRET_TOKEN_42".to_vec(),
        };
        let err = git_error("git credential approve", &output, Some("SECRET_TOKEN_42"));
        let msg = err.to_string();
        assert!(!msg.contains("SECRET_TOKEN_42"), "{msg}");
        assert!(msg.contains("***"), "{msg}");
    }
}
