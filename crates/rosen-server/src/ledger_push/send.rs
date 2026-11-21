//! One POST of a batch to the ledger endpoint, run by the `curl` binary.
//!
//! The mirror pushes through the `git` binary rather than a git library;
//! the push does the same with `curl`, so the server carries no HTTP or
//! certificate store of its own and uses the system's trust roots. The
//! token comes from git's credential helper, the one the mirror push already
//! uses, and reaches curl on its standard input, never on a command line
//! another user could read.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use super::cursor::Held;
use crate::import::git_command;

/// curl's own limit on one request; the process is stopped a little later
/// if curl itself hangs.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A credential helper that does not answer within this is given up on.
const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(60);
/// How much of an error response is kept for the event.
const SHOWN_RESPONSE: usize = 300;
/// Maximum same-origin 307/308 redirects followed for one request (#446).
const MAX_REDIRECTS: usize = 5;

/// A user name and token for HTTPS basic auth.
#[derive(Clone)]
pub struct Credential {
    user: String,
    token: String,
}

/// Leaves the token out of any debug output.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Why a send did not reach the endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendFailure {
    /// HTTP 410: the endpoint's repository is archived or deleted. Retrying
    /// cannot help, so the push stops until the endpoint changes.
    Gone,
    /// HTTP 301, 307 or 308: the endpoint moved to `location` (#446).
    Moved { status: u16, location: String },
    /// HTTP 409 `source_changed`: the endpoint holds feed state from another
    /// Rosen server (different `feed_id`). Sending stops until the operator
    /// runs `rosen collapse link --reset` (#445).
    SourceChanged { message: String },
    /// Anything else, retried with backoff. `status` is the HTTP status when
    /// the endpoint answered.
    Failed {
        status: Option<u16>,
        message: String,
    },
}

/// Whether `status` means the endpoint refused the batch itself (invalid item,
/// payload too large or unprocessable entity), so splitting by halves can
/// isolate refused items instead of retrying the same batch unchanged (#444).
pub fn is_batch_refusal(status: Option<u16>) -> bool {
    matches!(status, Some(400 | 413 | 422))
}

#[derive(Debug, PartialEq, Eq)]
struct Origin {
    scheme: &'static str,
    host: String,
    port: u16,
}

fn parse_origin(url: &str) -> Option<Origin> {
    if url.chars().any(char::is_control) {
        return None;
    }
    let (scheme_raw, rest) = url.split_once("://")?;
    let (scheme, default_port) = if scheme_raw.eq_ignore_ascii_case("https") {
        ("https", 443)
    } else if scheme_raw.eq_ignore_ascii_case("http") {
        ("http", 80)
    } else {
        return None;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(after_open) = authority.strip_prefix('[') {
        let (addr, tail) = after_open.split_once(']')?;
        if addr.is_empty() {
            return None;
        }
        let port = match tail {
            "" => default_port,
            _ => tail.strip_prefix(':')?.parse::<u16>().ok()?,
        };
        (format!("[{}]", addr.to_ascii_lowercase()), port)
    } else if let Some((host, port_str)) = authority.rsplit_once(':') {
        if host.is_empty() || host.contains(':') {
            return None;
        }
        let port = port_str.parse::<u16>().ok()?;
        (host.to_ascii_lowercase(), port)
    } else {
        (authority.to_ascii_lowercase(), default_port)
    };
    Some(Origin { scheme, host, port })
}

/// Whether `from` and `to` share exact scheme, host and port, with no embedded
/// credentials. Only same-origin 307/308 redirects may receive the push token.
pub fn same_origin(from: &str, to: &str) -> bool {
    match (parse_origin(from), parse_origin(to)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Extracts the Collapse repository root (`<origin>/<owner>/<name>`) from a
/// Collapse root, state feed, ledger push or action relay URL (#446).
pub fn collapse_root_from_url(url: &str) -> Option<String> {
    if url.chars().any(char::is_control) {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let (authority, path_and_query) = rest.split_once('/')?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let path = path_and_query
        .split(['?', '#'])
        .next()
        .unwrap_or(path_and_query)
        .trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    let (owner, name, tail) = (segments[0], segments[1], &segments[2..]);
    let valid_tail = matches!(
        tail,
        [] | ["ledgers"]
            | ["rosen", "state"]
            | ["rosen", "doorbell"]
            | ["rosen", "actions"]
            | ["rosen", "actions", _, "result"]
    );
    if !valid_tail || name.ends_with(".git") {
        return None;
    }
    Some(format!("{scheme}://{authority}/{owner}/{name}"))
}

/// Extracts `<owner>/<name>` from a Collapse URL, when it matches a Collapse
/// repository root or feed/relay route (#446).
pub fn collapse_repo_slug(url: &str) -> Option<String> {
    let root = collapse_root_from_url(url)?;
    let (_, rest) = root.split_once("://")?;
    let (_, slug) = rest.split_once('/')?;
    Some(slug.to_owned())
}

/// Operator hint for a redirected Collapse endpoint (#446).
pub fn moved_hint(location: &str) -> String {
    match collapse_repo_slug(location) {
        Some(slug) => format!("rosen collapse link {slug} --land"),
        None => format!("update `collapse` or `ledger_endpoint` in rosen.toml to {location}"),
    }
}

/// The credential git's helpers hold for `endpoint`, or `None` when they
/// hold none. Never prompts: a headless server has no one to answer.
pub async fn credential(data_dir: &Path, endpoint: &str) -> Option<Credential> {
    // A token never goes over plain http, which config allows only for a
    // loopback endpoint.
    if !endpoint.starts_with("https://") {
        return None;
    }
    let mut command = tokio::process::Command::from(git_command(data_dir));
    command
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    stdin
        .write_all(format!("url={endpoint}\n\n").as_bytes())
        .await
        .ok()?;
    drop(stdin);
    let output = tokio::time::timeout(CREDENTIAL_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_owned)
    };
    let token = field("password").filter(|token| !token.is_empty())?;
    // The endpoint takes any user name with the push token.
    let user = field("username").unwrap_or_else(|| "rosen".to_owned());
    Some(Credential { user, token })
}

/// POSTs `body` to `endpoint`, following same-origin 307/308 redirects, and
/// returns what the endpoint holds along with the first redirect status and
/// final URL when redirected (#446).
pub async fn post_following(
    scratch: &Path,
    endpoint: &str,
    body: &[u8],
    credential: Option<&Credential>,
) -> Result<(Held, Option<(u16, String)>), SendFailure> {
    let accepted = post_json_following(scratch, endpoint, body, credential).await?;
    let held = held(&accepted)?;
    Ok((held, accepted.redirected))
}

/// A 2xx answer from the endpoint.
#[derive(Debug)]
pub struct Accepted {
    pub status: u16,
    pub body: String,
    /// First redirect HTTP status and final URL when a same-origin 307/308
    /// redirect was followed (#446).
    pub redirected: Option<(u16, String)>,
}

impl Accepted {
    /// A failure for an answer the caller cannot read.
    pub fn unreadable(&self, why: &str) -> SendFailure {
        SendFailure::Failed {
            status: Some(self.status),
            message: format!("{why}: {}", shown(&self.body)),
        }
    }
}

/// POSTs a JSON `body` to `endpoint`. Shared by the ledger push and the
/// state feed: the same credential, curl settings and status handling.
pub async fn post_json(
    scratch: &Path,
    endpoint: &str,
    body: &[u8],
    credential: Option<&Credential>,
) -> Result<Accepted, SendFailure> {
    let fail = |message: String| SendFailure::Failed {
        status: None,
        message,
    };
    let config = curl_config(endpoint, credential)?;
    std::fs::create_dir_all(scratch)
        .map_err(|error| fail(format!("creating {}: {error}", scratch.display())))?;
    let body_file = tempfile::Builder::new()
        .prefix("batch-")
        .suffix(".json")
        .tempfile_in(scratch)
        .map_err(|error| fail(format!("writing the batch: {error}")))?;
    std::fs::write(body_file.path(), body)
        .map_err(|error| fail(format!("writing the batch: {error}")))?;
    let output = run_curl(Some(body_file.path()), &config).await?;
    status_answer(&output)
}

/// POSTs a JSON `body` to `endpoint`, following same-origin 307 and 308
/// redirects (which preserve POST and the request body) up to [`MAX_REDIRECTS`]
/// hops (#446). HTTP 301 and cross-origin redirects are returned as
/// [`SendFailure::Moved`] without leaking credentials.
pub async fn post_json_following(
    scratch: &Path,
    endpoint: &str,
    body: &[u8],
    credential: Option<&Credential>,
) -> Result<Accepted, SendFailure> {
    let mut current = endpoint.to_owned();
    let mut first_status: Option<u16> = None;
    for _ in 0..=MAX_REDIRECTS {
        match post_json(scratch, &current, body, credential).await {
            Ok(mut accepted) => {
                if let Some(status) = first_status {
                    accepted.redirected = Some((status, current));
                }
                return Ok(accepted);
            }
            Err(SendFailure::Moved { status, location })
                if matches!(status, 307 | 308)
                    && same_origin(&current, &location)
                    && location != current =>
            {
                first_status.get_or_insert(status);
                current = location;
            }
            Err(failure) => return Err(failure),
        }
    }
    Err(SendFailure::Failed {
        status: first_status,
        message: format!("too many redirects from {endpoint} (last: {current})"),
    })
}

/// GETs `endpoint` with the same credential and curl settings as a POST.
/// The action relay (R14) reads Collapse's doorbell and actions this way.
pub async fn get_json(
    endpoint: &str,
    credential: Option<&Credential>,
) -> Result<Accepted, SendFailure> {
    let config = curl_config(endpoint, credential)?;
    let output = run_curl(None, &config).await?;
    status_answer(&output)
}

/// GETs `endpoint`, following same-origin 307 and 308 redirects up to
/// [`MAX_REDIRECTS`] hops (#446).
pub async fn get_json_following(
    endpoint: &str,
    credential: Option<&Credential>,
) -> Result<Accepted, SendFailure> {
    let mut current = endpoint.to_owned();
    let mut first_status: Option<u16> = None;
    for _ in 0..=MAX_REDIRECTS {
        match get_json(&current, credential).await {
            Ok(mut accepted) => {
                if let Some(status) = first_status {
                    accepted.redirected = Some((status, current));
                }
                return Ok(accepted);
            }
            Err(SendFailure::Moved { status, location })
                if matches!(status, 307 | 308)
                    && same_origin(&current, &location)
                    && location != current =>
            {
                first_status.get_or_insert(status);
                current = location;
            }
            Err(failure) => return Err(failure),
        }
    }
    Err(SendFailure::Failed {
        status: first_status,
        message: format!("too many redirects from {endpoint} (last: {current})"),
    })
}

/// The endpoint's answer, from curl's output.
fn status_answer(output: &std::process::Output) -> Result<Accepted, SendFailure> {
    let fail = |message: String| SendFailure::Failed {
        status: None,
        message,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(fail(format!("curl: {}", stderr.trim())));
    }
    // `--write-out` puts `\n%{http_code}\n%{redirect_url}` after the response.
    let (before_redirect, redirect_url) = stdout.rsplit_once('\n').unwrap_or(("", &stdout));
    let (response, status) = before_redirect
        .rsplit_once('\n')
        .unwrap_or(("", before_redirect));
    let status: u16 = status
        .trim()
        .parse()
        .map_err(|_| fail(format!("curl printed no HTTP status: {}", shown(&stdout))))?;
    answer(status, response, redirect_url.trim())
}

/// curl's settings, read from its standard input. A token never appears
/// in the arguments, which other users can list.
fn curl_config(endpoint: &str, credential: Option<&Credential>) -> Result<String, SendFailure> {
    let mut config = format!("url = \"{}\"\n", quoted(endpoint));
    if let Some(credential) = credential {
        let pair = format!("{}:{}", credential.user, credential.token);
        if pair.chars().any(char::is_control) {
            return Err(SendFailure::Failed {
                status: None,
                message: "the credential helper's token holds a control character".to_owned(),
            });
        }
        config.push_str(&format!("user = \"{}\"\n", quoted(&pair)));
    }
    Ok(config)
}

/// Escapes text for a double-quoted curl config value.
fn quoted(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Runs curl: a POST of the file at `body`, or a GET without one.
async fn run_curl(body: Option<&Path>, config: &str) -> Result<std::process::Output, SendFailure> {
    let fail = |message: String| SendFailure::Failed {
        status: None,
        message,
    };
    let mut command = tokio::process::Command::new("curl");
    // `-q` first, so a `.curlrc` on the server cannot add options.
    command
        .args(["-q", "--silent", "--show-error", "--proto", "=https,http"])
        .args(["--max-redirs", "0"])
        .args(["--connect-timeout", &CONNECT_TIMEOUT.as_secs().to_string()])
        .args(["--max-time", &REQUEST_TIMEOUT.as_secs().to_string()]);
    if let Some(body) = body {
        let mut data = std::ffi::OsString::from("@");
        data.push(body.as_os_str());
        command
            .args(["--request", "POST"])
            .args(["--header", "Content-Type: application/json"])
            .arg("--data-binary")
            .arg(data);
    }
    command
        .args([
            "--write-out",
            "\\n%{http_code}\\n%{redirect_url}",
            "--config",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| fail(format!("could not run curl: {error}")))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    stdin
        .write_all(config.as_bytes())
        .await
        .map_err(|error| fail(format!("curl: {error}")))?;
    drop(stdin);
    let limit = REQUEST_TIMEOUT + CONNECT_TIMEOUT;
    tokio::time::timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| fail(format!("curl did not finish within {limit:?}")))?
        .map_err(|error| fail(format!("curl: {error}")))
}

/// Reads the endpoint's status: its answer when it accepted, or why not.
fn answer(status: u16, response: &str, redirect_url: &str) -> Result<Accepted, SendFailure> {
    if status == 410 {
        return Err(SendFailure::Gone);
    }
    if matches!(status, 301 | 307 | 308)
        && !redirect_url.is_empty()
        && !redirect_url.chars().any(char::is_control)
    {
        return Err(SendFailure::Moved {
            status,
            location: redirect_url.to_owned(),
        });
    }
    if status == 409 && is_source_changed(response) {
        return Err(SendFailure::SourceChanged {
            message: source_changed_message(response),
        });
    }
    let refuse = |message: String| SendFailure::Failed {
        status: Some(status),
        message,
    };
    if !(200..300).contains(&status) {
        let hint = if status == 401 || status == 403 {
            "; check the push token git's credential helper holds for the endpoint"
        } else {
            ""
        };
        return Err(refuse(format!("HTTP {status}: {}{hint}", shown(response))));
    }
    Ok(Accepted {
        status,
        body: response.to_owned(),
        redirected: None,
    })
}

fn is_source_changed(response: &str) -> bool {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(response) {
        for key in ["error", "code", "reason"] {
            if value.get(key).and_then(serde_json::Value::as_str) == Some("source_changed") {
                return true;
            }
        }
    }
    response.contains("source_changed")
}

fn source_changed_message(response: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(response)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("detail"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .filter(|text| !text.trim().is_empty());
    match detail {
        Some(text) => format!(
            "source_changed ({text}); run `rosen collapse link --reset` to replace Collapse's feed state"
        ),
        None => "source_changed: Collapse holds feed state from another Rosen server; run `rosen collapse link --reset` to replace Collapse's feed state".to_owned(),
    }
}

/// What the ledger endpoint holds, from its answer.
fn held(accepted: &Accepted) -> Result<Held, SendFailure> {
    // `{"highest_sequence":{"discovery":N,"past_work":M}}`; a ledger it
    // leaves out holds nothing.
    let receipt: serde_json::Value = serde_json::from_str(&accepted.body)
        .map_err(|error| accepted.unreadable(&format!("unreadable answer ({error})")))?;
    let highest = receipt
        .get("highest_sequence")
        .filter(|highest| highest.is_object())
        .ok_or_else(|| accepted.unreadable("answer names no highest_sequence"))?;
    let held = |ledger: &str| highest.get(ledger).and_then(serde_json::Value::as_u64);
    Ok(Held {
        discovery: held("discovery").unwrap_or(0),
        past_work: held("past_work").unwrap_or(0),
    })
}

fn shown(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(SHOWN_RESPONSE) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Risk (#446): following a cross-origin redirect would leak the Collapse
    /// push token to another host or over plain HTTP.
    #[test]
    fn same_origin_and_redirect_helpers_reject_cross_origin_and_extract_slug() {
        assert!(same_origin(
            "https://api.collapse.sh/old/repo/rosen/state",
            "https://API.COLLAPSE.SH:443/new/repo/rosen/state"
        ));
        assert!(same_origin(
            "http://127.0.0.1:8080/old/repo/ledgers",
            "http://127.0.0.1:8080/new/repo/ledgers"
        ));
        assert!(!same_origin(
            "https://api.collapse.sh/old/repo/rosen/state",
            "http://api.collapse.sh/new/repo/rosen/state"
        ));
        assert!(!same_origin(
            "https://api.collapse.sh/old/repo/rosen/state",
            "https://evil.example/new/repo/rosen/state"
        ));
        assert!(!same_origin(
            "https://api.collapse.sh/old/repo/rosen/state",
            "https://tok@api.collapse.sh/new/repo/rosen/state"
        ));
        assert_eq!(
            moved_hint("https://api.collapse.sh/new-org/new-repo/rosen/state"),
            "rosen collapse link new-org/new-repo --land"
        );
        assert_eq!(
            moved_hint("https://api.collapse.sh/new-org/new-repo/ledgers"),
            "rosen collapse link new-org/new-repo --land"
        );
        assert_eq!(
            collapse_root_from_url(
                "https://api.collapse.sh/new-org/new-repo/rosen/actions?after=5"
            ),
            Some("https://api.collapse.sh/new-org/new-repo".to_owned())
        );
        for code in [301, 307, 308] {
            assert_eq!(
                answer(code, "", "https://api.collapse.sh/new/repo/rosen/state").unwrap_err(),
                SendFailure::Moved {
                    status: code,
                    location: "https://api.collapse.sh/new/repo/rosen/state".to_owned(),
                }
            );
        }
        assert!(matches!(
            answer(307, "", "").unwrap_err(),
            SendFailure::Failed {
                status: Some(307),
                ..
            }
        ));
    }
}
