//! Read-only end-to-end verification of a server's Collapse link (SPEC R1, R10, #435).
//!
//! Probes every piece needed for Collapse without writing to SQLite, staging
//! repositories, or in-memory proactive-auth state, and never exposes
//! credential tokens or user names.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rosen_core::ObjectId;
use rosen_protocol::{
    CollapseCheckItem, CollapseCheckReport, CollapseCheckStatus, ErrorBody, ErrorCode, Outcome,
    RelayKind, RelaySetting, Response, COLLAPSE_MACHINE,
};
use rosen_store::LooseObjectStore;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::import::git_command;
use crate::ledger_push::send::{self, Credential, SendFailure};
use crate::mirror::{hide_credentials, proactive_auth};
use crate::{ServerConfig, ServerState};

/// Timeout for one `git ls-remote` probe.
const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for one local `git config` read.
const CONFIG_TIMEOUT: Duration = Duration::from_secs(15);

/// Empty state feed batch (SPEC R9): stores nothing and reads `highest_sequence`.
const STATE_PROBE_BODY: &[u8] = br#"{"format":1,"through":0,"changes":[],"conflicts":[],"threads":[],"coverage":[],"config":[],"events":[],"machines":[],"presence":[],"channel":[]}"#;

/// Empty ledger batch (SPEC R13): stores nothing and reads `highest_sequence`.
const LEDGER_PROBE_BODY: &[u8] = br#"{"entries":[]}"#;

/// Read-only snapshot of the server's Collapse-relevant configuration.
struct ServerSnapshot {
    has_base: bool,
    base_branch: Option<String>,
    config_error: Option<String>,
    collapse: Option<String>,
    state_url: Option<String>,
    ledger_url: Option<String>,
    mirror_remote: Option<String>,
    mirror_from_config: bool,
    mirror_owner: bool,
    mirror_proactive_auth: bool,
    relay: RelaySetting,
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedRemote {
    Repo { owner: String, name: String },
    LocalPath,
    Malformed(String),
}

/// RPC entry point for `Request::CollapseCheck`.
pub async fn handle(state: &Arc<ServerState>) -> Outcome {
    match run(state).await {
        Ok(report) => Outcome::Ok(Response::CollapseCheck(Box::new(report))),
        Err(error) => Outcome::Error(ErrorBody::new(ErrorCode::Internal, error)),
    }
}

/// Runs all Collapse link probes against `state` without mutating server state.
pub async fn run(state: &Arc<ServerState>) -> Result<CollapseCheckReport, String> {
    let config = state.config.clone();
    let proactive = state.mirror_proactive_auth.is_enabled();
    let snapshot = tokio::task::spawn_blocking(move || read_snapshot(&config, proactive))
        .await
        .map_err(|join| join.to_string())??;
    let checks = run_checks(&state.config, &snapshot).await;
    let ok = checks
        .iter()
        .all(|item| item.status != CollapseCheckStatus::Fail);
    let mirror_remote = snapshot
        .mirror_remote
        .as_deref()
        .map(|remote| hide_credentials(remote, remote));

    Ok(CollapseCheckReport {
        ok,
        mirror_remote,
        collapse: snapshot.collapse,
        ledger_endpoint: snapshot.ledger_url,
        mirror_owner: snapshot.mirror_owner,
        mirror_proactive_auth: snapshot.mirror_proactive_auth,
        checks,
    })
}

async fn run_checks(config: &ServerConfig, snapshot: &ServerSnapshot) -> Vec<CollapseCheckItem> {
    let git_dir = existing_dir(&config.data_dir);
    let cred_use_http_path = check_credential_use_http_path(snapshot, &git_dir).await;
    let (cred_mirror, _) = check_credential(
        "credential_mirror",
        snapshot.mirror_remote.as_deref(),
        "mirror remote",
        &git_dir,
    )
    .await;
    let (cred_state, state_cred) = check_credential(
        "credential_state",
        snapshot.state_url.as_deref(),
        "collapse root",
        &git_dir,
    )
    .await;
    let (cred_ledgers, ledger_cred) = check_credential(
        "credential_ledgers",
        snapshot.ledger_url.as_deref(),
        "ledger endpoint",
        &git_dir,
    )
    .await;
    vec![
        check_mirror_owner(&config.data_dir, snapshot.mirror_owner),
        check_collapse_config(snapshot),
        check_repo_match(snapshot),
        cred_use_http_path,
        cred_mirror,
        cred_state,
        cred_ledgers,
        check_mirror_ls_remote(config, snapshot, &git_dir).await,
        check_state_endpoint(snapshot.state_url.as_deref(), state_cred.as_ref()).await,
        check_ledger_endpoint(snapshot.ledger_url.as_deref(), ledger_cred.as_ref()).await,
        check_relay_allow(&snapshot.relay),
        check_relay_doorbell(
            snapshot.collapse.as_deref(),
            snapshot.state_url.as_deref(),
            state_cred.as_ref(),
        )
        .await,
    ]
}

fn read_snapshot(config: &ServerConfig, proactive: bool) -> Result<ServerSnapshot, String> {
    let mirror_owner = crate::mirror::owner::is_owner(&config.data_dir);
    let mut snapshot = ServerSnapshot {
        has_base: false,
        base_branch: None,
        config_error: None,
        collapse: None,
        state_url: None,
        ledger_url: None,
        mirror_remote: None,
        mirror_from_config: false,
        mirror_owner,
        mirror_proactive_auth: proactive,
        relay: RelaySetting::default(),
    };
    let metadata = config.metadata_path();
    if !metadata.is_file() {
        return Ok(snapshot);
    }
    let conn = open_read_only(&metadata)?;
    let stored_mirror = read_stored_mirror(&conn)?;
    snapshot.relay = read_relay_setting(&conn)?;
    if let Some((branch, commit)) = read_base_ref(&conn)? {
        snapshot.has_base = true;
        snapshot.base_branch = Some(branch);
        if config.objects_dir().is_dir() {
            let store = LooseObjectStore::open(config.objects_dir()).map_err(|e| e.to_string())?;
            let resolved =
                crate::project_config::resolve(&store, &commit).map_err(|e| e.to_string())?;
            snapshot.config_error = resolved.error;
            snapshot.collapse = resolved.config.collapse.clone();
            snapshot.state_url = resolved.config.state_feed_url();
            snapshot.ledger_url = resolved.config.ledger_push_url();
            if let Some(remote) = resolved.config.mirror.remote.clone() {
                snapshot.mirror_remote = Some(remote);
                snapshot.mirror_from_config = true;
            }
        }
    }
    if snapshot.mirror_remote.is_none() {
        snapshot.mirror_remote = stored_mirror;
    }
    Ok(snapshot)
}

fn open_read_only(path: &Path) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn optional_table<T>(result: rusqlite::Result<Option<T>>) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.to_string().contains("no such table") => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn read_base_ref(conn: &Connection) -> Result<Option<(String, ObjectId)>, String> {
    let row = optional_table(
        conn.query_row("SELECT name, target FROM base_refs LIMIT 1", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .optional(),
    )?;
    match row {
        Some((branch, hex)) => {
            let commit = ObjectId::from_hex(&hex).map_err(|e| e.to_string())?;
            Ok(Some((branch, commit)))
        }
        None => Ok(None),
    }
}

fn read_stored_mirror(conn: &Connection) -> Result<Option<String>, String> {
    optional_table(
        conn.query_row("SELECT remote FROM mirror WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional(),
    )
}

fn read_relay_setting(conn: &Connection) -> Result<RelaySetting, String> {
    let allow: Option<String> = optional_table(
        conn.query_row("SELECT allow FROM relay_setting WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional(),
    )?;
    let status: Option<String> = optional_table(
        conn.query_row(
            "SELECT status FROM machines WHERE id = ?1",
            [COLLAPSE_MACHINE],
            |row| row.get(0),
        )
        .optional(),
    )?;
    let allow = allow
        .unwrap_or_default()
        .split(',')
        .filter_map(RelayKind::parse)
        .collect();
    Ok(RelaySetting {
        allow,
        revoked: status.as_deref() == Some("revoked"),
    })
}

fn check_mirror_owner(data_dir: &Path, is_owner: bool) -> CollapseCheckItem {
    if is_owner {
        pass("mirror_owner", "this server is marked as the mirror owner")
    } else {
        fail(
            "mirror_owner",
            "this server is not marked as the mirror owner; mirror pushes, Collapse state feed, ledger push and action relay are disabled",
            format!(
                "start rosen-server with --mirror-owner (or set ROSEN_MIRROR_OWNER={})",
                data_dir.display()
            ),
        )
    }
}

fn check_collapse_config(snapshot: &ServerSnapshot) -> CollapseCheckItem {
    if !snapshot.has_base {
        return fail(
            "collapse_config",
            "no project base imported yet",
            "import the repository with `rosen import <repo> --mirror <remote>`",
        );
    }
    let Some(root) = snapshot.collapse.as_deref() else {
        let detail = match &snapshot.config_error {
            Some(error) => {
                format!("base rosen.toml is invalid ({error}) and no `collapse` root is in force")
            }
            None => "no `collapse` root is configured in the base's rosen.toml".to_owned(),
        };
        return fail(
            "collapse_config",
            detail,
            "add `collapse = \"https://api.collapse.sh/<owner>/<name>\"` to rosen.toml and land it",
        );
    };
    if let Err(why) = parse_collapse_repo(root) {
        return fail(
            "collapse_config",
            format!("collapse = {root} ({why})"),
            "set `collapse = \"https://api.collapse.sh/<owner>/<name>\"` in rosen.toml and land it",
        );
    }
    let detail = match &snapshot.config_error {
        Some(error) => format!("collapse = {root} (base rosen.toml refused: {error})"),
        None => format!("collapse = {root}"),
    };
    pass("collapse_config", detail)
}

fn check_repo_match(snapshot: &ServerSnapshot) -> CollapseCheckItem {
    let Some(collapse) = snapshot.collapse.as_deref() else {
        return skipped("repo_match", "no `collapse` root configured", None);
    };
    let Some(mirror) = snapshot.mirror_remote.as_deref() else {
        return fail(
            "repo_match",
            "no mirror remote is configured",
            "add `[mirror]\\nremote = \"https://.../<owner>/<name>.git\"` to rosen.toml and land it",
        );
    };
    let Ok((c_owner, c_name)) = parse_collapse_repo(collapse) else {
        return skipped(
            "repo_match",
            "collapse root does not name an <owner>/<name> repository",
            None,
        );
    };
    match parse_mirror_repo(mirror) {
        ParsedRemote::LocalPath => skipped(
            "repo_match",
            "mirror remote is a local path; cannot compare owner and name with collapse",
            None,
        ),
        ParsedRemote::Malformed(why) => fail(
            "repo_match",
            format!(
                "mirror remote {} does not name an <owner>/<name> repository ({why})",
                hide_credentials(mirror, mirror)
            ),
            "set `[mirror].remote` in rosen.toml to `https://<host>/<owner>/<name>.git` and land it",
        ),
        ParsedRemote::Repo { owner, name } if owner == c_owner && name == c_name => {
            let source = if snapshot.mirror_from_config {
                "[mirror]"
            } else {
                "stored mirror remote"
            };
            pass(
                "repo_match",
                format!("collapse and {source} both point to {c_owner}/{c_name}"),
            )
        }
        ParsedRemote::Repo { owner, name } => fail(
            "repo_match",
            format!(
                "collapse points to {c_owner}/{c_name}, but mirror remote points to {owner}/{name}"
            ),
            "update `collapse` or `[mirror].remote` in rosen.toml so both point to the same <owner>/<name>, then land it",
        ),
    }
}

/// Verifies that git keys HTTPS credentials by full path (`credential.useHttpPath = true`)
/// for every configured Collapse/mirror HTTPS URL so linking multiple repositories on one
/// host keeps each repository's push token isolated (SPEC R1, #442).
async fn check_credential_use_http_path(
    snapshot: &ServerSnapshot,
    git_dir: &Path,
) -> CollapseCheckItem {
    let urls = https_credential_urls(snapshot);
    if urls.is_empty() {
        return skipped(
            "credential_use_http_path",
            "no HTTPS Collapse or mirror URL configured",
            None,
        );
    }
    let (mut checked, mut missing) = (Vec::new(), Vec::new());
    for url in urls {
        let Some(origin) = https_origin(url) else {
            continue;
        };
        push_unique(&mut checked, &origin);
        match url_has_use_http_path(git_dir, url).await {
            Ok(true) => {}
            Ok(false) => push_unique(&mut missing, &origin),
            Err(error) => {
                return fail(
                    "credential_use_http_path",
                    format!("could not read git credential.useHttpPath for {origin}: {error}"),
                    format!("git config --global credential.{origin}.useHttpPath true"),
                );
            }
        }
    }
    finish_use_http_path_check(&checked, &missing)
}

fn https_credential_urls(snapshot: &ServerSnapshot) -> Vec<&str> {
    [
        snapshot.mirror_remote.as_deref(),
        snapshot.state_url.as_deref(),
        snapshot.ledger_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|url| proactive_auth::is_https(url))
    .collect()
}

fn finish_use_http_path_check(checked: &[String], missing: &[String]) -> CollapseCheckItem {
    if checked.is_empty() {
        return skipped(
            "credential_use_http_path",
            "no HTTPS Collapse or mirror origin configured",
            None,
        );
    }
    if missing.is_empty() {
        return pass(
            "credential_use_http_path",
            format!("credential.useHttpPath is true for {}", checked.join(", ")),
        );
    }
    let hints: Vec<String> = missing
        .iter()
        .map(|o| format!("git config --global credential.{o}.useHttpPath true"))
        .collect();
    fail(
        "credential_use_http_path",
        format!(
            "credential.useHttpPath is unset for {}; linking a second repository on the same host overwrites the first token",
            missing.join(", ")
        ),
        hints.join(" && "),
    )
}

async fn url_has_use_http_path(git_dir: &Path, url: &str) -> Result<bool, String> {
    let mut command = tokio::process::Command::from(git_command(git_dir));
    command
        .args([
            "config",
            "--bool",
            "--get-urlmatch",
            "credential.useHttpPath",
            url,
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(CONFIG_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("git config did not finish within {CONFIG_TIMEOUT:?}"))?
        .map_err(|error| format!("could not run git: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout).trim() == "true"),
        // `git config --get-urlmatch` exits 1 when no matching key is configured.
        Some(1) => Ok(false),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let cleaned = hide_credentials(url, stderr.trim());
            Err(if cleaned.is_empty() {
                format!("git exited with {}", output.status)
            } else {
                cleaned
            })
        }
    }
}

fn https_origin(url: &str) -> Option<String> {
    if !proactive_auth::is_https(url) {
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

fn push_unique(values: &mut Vec<String>, item: &str) {
    if !values.iter().any(|existing| existing == item) {
        values.push(item.to_owned());
    }
}

async fn check_credential(
    name: &str,
    url: Option<&str>,
    missing_label: &str,
    git_dir: &Path,
) -> (CollapseCheckItem, Option<Credential>) {
    let Some(url) = url else {
        return (
            skipped(name, format!("no {missing_label} configured"), None),
            None,
        );
    };
    if !proactive_auth::is_https(url) {
        let reason = if url.starts_with("http://") {
            "plain http loopback endpoint does not use git credentials"
        } else {
            "non-HTTPS mirror remote does not use HTTP git credentials"
        };
        return (skipped(name, reason, None), None);
    }
    let shown = hide_credentials(url, url);
    match send::credential(git_dir, url).await {
        Some(cred) => (pass(name, format!("token present for {shown}")), Some(cred)),
        None => (
            fail(
                name,
                format!("no token in git credential helper for {shown}"),
                credential_approve_hint(&shown),
            ),
            None,
        ),
    }
}

async fn check_mirror_ls_remote(
    config: &ServerConfig,
    snapshot: &ServerSnapshot,
    git_dir: &Path,
) -> CollapseCheckItem {
    let Some(raw_remote) = snapshot.mirror_remote.as_deref() else {
        return skipped("mirror_ls_remote", "no mirror remote configured", None);
    };
    let resolved = match resolve_remote_name(config, raw_remote) {
        Ok(remote) => remote,
        Err(error) => {
            return fail(
                "mirror_ls_remote",
                format!("could not resolve mirror remote: {error}"),
                "check `[mirror].remote` in rosen.toml",
            );
        }
    };
    let shown = hide_credentials(raw_remote, raw_remote);
    let full_ref = snapshot
        .base_branch
        .as_deref()
        .map(|branch| format!("refs/heads/{branch}"));
    let mut args = vec!["ls-remote", "--", resolved.as_str()];
    if let Some(full) = full_ref.as_deref() {
        args.push(full);
    }
    if !proactive_auth::is_https(&resolved) {
        return match run_ls_remote(git_dir, &args, &resolved, false).await {
            Ok(()) => pass(
                "mirror_ls_remote",
                format!(
                    "ls-remote succeeded for {shown} (non-HTTPS; proactive auth not applicable)"
                ),
            ),
            Err(error) => fail(
                "mirror_ls_remote",
                format!("ls-remote failed for {shown}: {error}"),
                "check that the mirror remote exists and is reachable from the server",
            ),
        };
    }
    check_https_ls_remote(
        git_dir,
        &args,
        &resolved,
        &shown,
        snapshot.mirror_proactive_auth,
    )
    .await
}

async fn check_https_ls_remote(
    git_dir: &Path,
    args: &[&str],
    remote: &str,
    shown: &str,
    cached_proactive: bool,
) -> CollapseCheckItem {
    let without = run_ls_remote(git_dir, args, remote, false).await;
    let with = if proactive_auth::git_supports_proactive_auth().await {
        Some(run_ls_remote(git_dir, args, remote, true).await)
    } else {
        None
    };
    classify_https_ls_remote(without, with, shown, cached_proactive)
}

fn classify_https_ls_remote(
    without: Result<(), String>,
    with: Option<Result<(), String>>,
    shown: &str,
    cached_proactive: bool,
) -> CollapseCheckItem {
    let suffix = if cached_proactive {
        " (server proactive auth active)"
    } else {
        ""
    };
    match (without, with) {
        (Ok(()), Some(Ok(()))) => pass(
            "mirror_ls_remote",
            format!("ls-remote succeeded without and with proactive auth{suffix}"),
        ),
        (Ok(()), None) => pass(
            "mirror_ls_remote",
            "ls-remote succeeded without proactive auth (git < 2.46; proactive auth unavailable)",
        ),
        (Ok(()), Some(Err(with_err))) => fail(
            "mirror_ls_remote",
            format!("ls-remote succeeded without proactive auth, but failed with -c http.proactiveAuth=basic: {with_err}"),
            format!("the stored token for {shown} may be invalid; refresh it: {}", credential_approve_hint(shown)),
        ),
        (Err(without_err), Some(Ok(()))) => {
            let reason = if proactive_auth::is_not_found(&without_err) {
                "got not-found (private host)"
            } else {
                "failed"
            };
            pass(
                "mirror_ls_remote",
                format!("ls-remote without proactive auth {reason}; retry with -c http.proactiveAuth=basic succeeded{suffix}"),
            )
        }
        (Err(without_err), None) if proactive_auth::is_not_found(&without_err) => fail(
            "mirror_ls_remote",
            proactive_auth::OLD_GIT_MESSAGE,
            format!("upgrade git to 2.46 or later on the server, or run `git config --global http.{shown}.proactiveAuth basic`"),
        ),
        (Err(without_err), None) => fail(
            "mirror_ls_remote",
            format!("ls-remote failed for {shown}: {without_err}"),
            credential_approve_hint(shown),
        ),
        (Err(without_err), Some(Err(with_err))) => fail(
            "mirror_ls_remote",
            format!("ls-remote failed without proactive auth ({without_err}) and with proactive auth ({with_err})"),
            format!("check that the repository exists and store a valid token: {}", credential_approve_hint(shown)),
        ),
    }
}

async fn run_ls_remote(
    git_dir: &Path,
    args: &[&str],
    remote: &str,
    proactive: bool,
) -> Result<(), String> {
    let mut command = tokio::process::Command::from(git_command(git_dir));
    if proactive {
        command.args(proactive_auth::PROACTIVE_AUTH_ARGS);
    }
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(LS_REMOTE_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("git ls-remote did not finish within {LS_REMOTE_TIMEOUT:?}"))?
        .map_err(|error| format!("could not run git: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let cleaned = hide_credentials(remote, stderr.trim());
        Err(if cleaned.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            cleaned
        })
    }
}

async fn check_state_endpoint(
    url: Option<&str>,
    credential: Option<&Credential>,
) -> CollapseCheckItem {
    let Some(url) = url else {
        return skipped("state_endpoint", "no `collapse` root configured", None);
    };
    let scratch = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => {
            return fail(
                "state_endpoint",
                format!("could not create temporary directory: {error}"),
                "check server temporary directory permissions",
            );
        }
    };
    match send::post_json(scratch.path(), url, STATE_PROBE_BODY, credential).await {
        Ok(accepted) => match serde_json::from_str::<serde_json::Value>(&accepted.body)
            .ok()
            .and_then(|v| v.get("highest_sequence")?.as_u64())
        {
            Some(highest) => pass(
                "state_endpoint",
                format!(
                    "reachable at {url} (HTTP {}, highest_sequence {highest})",
                    accepted.status
                ),
            ),
            None => fail(
                "state_endpoint",
                format!(
                    "unexpected answer from {url} (HTTP {}): missing highest_sequence",
                    accepted.status
                ),
                "verify that `collapse` in rosen.toml points to a Collapse repository root",
            ),
        },
        Err(failure) => endpoint_failure("state_endpoint", url, url, credential, failure),
    }
}

async fn check_ledger_endpoint(
    url: Option<&str>,
    credential: Option<&Credential>,
) -> CollapseCheckItem {
    let Some(url) = url else {
        return skipped(
            "ledger_endpoint",
            "no `ledger_endpoint` or `collapse` root configured",
            None,
        );
    };
    let scratch = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => {
            return fail(
                "ledger_endpoint",
                format!("could not create temporary directory: {error}"),
                "check server temporary directory permissions",
            );
        }
    };
    match send::post(scratch.path(), url, LEDGER_PROBE_BODY, credential).await {
        Ok(held) => pass(
            "ledger_endpoint",
            format!(
                "reachable at {url} (highest_sequence: discovery {}, past_work {})",
                held.discovery, held.past_work
            ),
        ),
        Err(failure) => endpoint_failure("ledger_endpoint", url, url, credential, failure),
    }
}

fn check_relay_allow(setting: &RelaySetting) -> CollapseCheckItem {
    if setting.revoked {
        return fail(
            "relay_allow",
            "the `collapse` machine is revoked; relayed actions are off",
            "re-enroll with `rosen server relay --allow approve,land,mark,config`",
        );
    }
    if setting.allow.is_empty() {
        return skipped(
            "relay_allow",
            "relay is off (no action kinds allowed)",
            Some(
                "enable with `rosen server relay --allow approve,land,mark,config` if you want Collapse actions"
                    .to_owned(),
            ),
        );
    }
    let kinds: Vec<&str> = setting.allow.iter().map(|kind| kind.as_str()).collect();
    pass("relay_allow", format!("allows {}", kinds.join(", ")))
}

async fn check_relay_doorbell(
    collapse: Option<&str>,
    state_url: Option<&str>,
    credential: Option<&Credential>,
) -> CollapseCheckItem {
    let Some(root) = collapse else {
        return skipped("relay_doorbell", "no `collapse` root configured", None);
    };
    let url = format!("{}/rosen/doorbell", root.trim_end_matches('/'));
    let cred_url = state_url.unwrap_or(&url);
    match send::get_json(&url, credential).await {
        Ok(accepted) => match serde_json::from_str::<serde_json::Value>(&accepted.body)
            .ok()
            .and_then(|v| v.get("generation")?.as_i64())
        {
            Some(generation) => pass(
                "relay_doorbell",
                format!(
                    "reachable at {url} (HTTP {}, generation {generation})",
                    accepted.status
                ),
            ),
            None => fail(
                "relay_doorbell",
                format!(
                    "unexpected doorbell answer from {url} (HTTP {}): missing generation",
                    accepted.status
                ),
                "verify that `collapse` in rosen.toml points to a Collapse repository root",
            ),
        },
        Err(failure) => endpoint_failure("relay_doorbell", &url, cred_url, credential, failure),
    }
}

fn endpoint_failure(
    name: &str,
    url: &str,
    credential_url: &str,
    credential: Option<&Credential>,
    failure: SendFailure,
) -> CollapseCheckItem {
    match failure {
        SendFailure::Gone => fail(
            name,
            format!("HTTP 410 from {url}: repository is archived or deleted"),
            "unarchive the repository on Collapse or update `collapse` in rosen.toml",
        ),
        SendFailure::Failed { status, message } => {
            let detail = match (credential, status) {
                (None, Some(401 | 403 | 404)) if proactive_auth::is_https(credential_url) => {
                    format!(
                        "{message} (git's credential helper holds no token for {credential_url})"
                    )
                }
                _ => message,
            };
            let hint = match status {
                Some(401 | 403) => credential_approve_hint(credential_url),
                Some(404) if credential.is_none() && proactive_auth::is_https(credential_url) => {
                    format!(
                        "check the repository path in rosen.toml and store a push token: {}",
                        credential_approve_hint(credential_url)
                    )
                }
                Some(404) => {
                    "check that the repository exists on Collapse and `collapse` in rosen.toml has the right owner/name"
                        .to_owned()
                }
                _ => format!("check network access to {url} and `collapse` in rosen.toml"),
            };
            fail(name, detail, hint)
        }
    }
}

fn parse_collapse_repo(url: &str) -> Result<(String, String), String> {
    let (_, rest) = url
        .split_once("://")
        .ok_or_else(|| "missing URL scheme".to_owned())?;
    let (_, path) = rest
        .split_once('/')
        .ok_or_else(|| "missing repository path".to_owned())?;
    let path = strip_query_and_fragment(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 2 {
        return Err(format!(
            "expected `<origin>/<owner>/<name>`, got `/{}`",
            segments.join("/")
        ));
    }
    if segments[1].ends_with(".git") {
        return Err("collapse root must not end with `.git`".to_owned());
    }
    Ok((segments[0].to_owned(), segments[1].to_owned()))
}

fn parse_mirror_repo(remote: &str) -> ParsedRemote {
    let path = if let Some((scheme, rest)) = remote.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return ParsedRemote::LocalPath;
        }
        let Some((_, path)) = rest.split_once('/') else {
            return ParsedRemote::Malformed("missing repository path".to_owned());
        };
        strip_query_and_fragment(path)
    } else if let Some(path) = scp_path(remote) {
        path
    } else {
        return ParsedRemote::LocalPath;
    };
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 2 {
        return ParsedRemote::Malformed(format!(
            "expected `<owner>/<name>[.git]`, got `/{}`",
            segments.join("/")
        ));
    }
    let name = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    if name.is_empty() {
        return ParsedRemote::Malformed("empty repository name".to_owned());
    }
    ParsedRemote::Repo {
        owner: segments[0].to_owned(),
        name: name.to_owned(),
    }
}

fn scp_path(remote: &str) -> Option<&str> {
    let (authority, path) = match remote.find("]:") {
        Some(end) => (&remote[..=end], &remote[end + 2..]),
        None => remote.split_once(':')?,
    };
    if authority.contains(['/', '\\']) || path.is_empty() || path.starts_with(':') {
        return None;
    }
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    (host.chars().count() >= 2).then_some(path)
}

fn strip_query_and_fragment(path: &str) -> &str {
    let end = path.find(['?', '#']).unwrap_or(path.len());
    &path[..end]
}

fn credential_approve_hint(url: &str) -> String {
    format!("printf 'url={url}\\nusername=rosen\\npassword=<token>\\n\\n' | git credential approve")
}

fn resolve_remote_name(config: &ServerConfig, remote: &str) -> std::io::Result<String> {
    if !Path::new(remote).is_absolute() && !remote.contains(':') {
        return Ok(std::path::absolute(config.mirror_staging_dir())?
            .join(remote)
            .to_string_lossy()
            .into_owned());
    }
    Ok(remote.to_owned())
}

fn existing_dir(data_dir: &Path) -> std::path::PathBuf {
    if data_dir.is_dir() {
        data_dir.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

fn pass(name: &str, detail: impl Into<String>) -> CollapseCheckItem {
    CollapseCheckItem {
        name: name.to_owned(),
        status: CollapseCheckStatus::Pass,
        detail: detail.into(),
        hint: None,
    }
}

fn fail(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> CollapseCheckItem {
    CollapseCheckItem {
        name: name.to_owned(),
        status: CollapseCheckStatus::Fail,
        detail: detail.into(),
        hint: Some(hint.into()),
    }
}

fn skipped(name: &str, detail: impl Into<String>, hint: Option<String>) -> CollapseCheckItem {
    CollapseCheckItem {
        name: name.to_owned(),
        status: CollapseCheckStatus::Skipped,
        detail: detail.into(),
        hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_collapse_and_mirror_owner_name_pairs() {
        assert_eq!(
            parse_collapse_repo("https://api.collapse.sh/mESmaC/rosen"),
            Ok(("mESmaC".to_owned(), "rosen".to_owned()))
        );
        assert_eq!(
            parse_collapse_repo("https://api.collapse.sh/mESmaC/rosen/"),
            Ok(("mESmaC".to_owned(), "rosen".to_owned()))
        );
        assert!(parse_collapse_repo("https://api.collapse.sh/mESmaC/rosen.git").is_err());
        assert!(parse_collapse_repo("https://api.collapse.sh/mESmaC/rosen/rosen/state").is_err());

        for remote in [
            "https://collapse.sh/mESmaC/rosen.git",
            "https://api.collapse.sh/mESmaC/rosen.git/",
            "https://github.com/mESmaC/rosen",
            "ssh://git@collapse.sh:22/mESmaC/rosen.git",
            "git://collapse.sh/mESmaC/rosen.git",
            "git@collapse.sh:mESmaC/rosen.git",
            "git@[2001:db8::1]:mESmaC/rosen.git",
        ] {
            assert_eq!(
                parse_mirror_repo(remote),
                ParsedRemote::Repo {
                    owner: "mESmaC".to_owned(),
                    name: "rosen".to_owned(),
                },
                "failed for {remote}"
            );
        }
        for local in [
            "/tmp/mESmaC/rosen.git",
            r"C:\repos\rosen.git",
            "file:///tmp/rosen.git",
        ] {
            assert_eq!(parse_mirror_repo(local), ParsedRemote::LocalPath, "{local}");
        }
    }

    fn setup_credential_store(dir: &Path, cred_file: &Path) {
        let cred_arg = cred_file.to_string_lossy().replace('\\', "/");
        let run_git = |args: &[&str]| {
            let status = crate::import::git_command(dir)
                .args(args)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "--quiet"]);
        run_git(&["config", "--local", "credential.helper", ""]);
        run_git(&[
            "config",
            "--local",
            "--add",
            "credential.helper",
            &format!("store --file={cred_arg}"),
        ]);
        run_git(&["config", "--local", "credential.useHttpPath", "true"]);
    }

    /// Risk (SPEC R1, #435): removing the `/rosen/state` credential must fail
    /// only `credential_state` with a `git credential approve` hint and never
    /// print any token.
    #[tokio::test]
    async fn credential_check_isolates_missing_state_token_and_never_leaks_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let cred_file = dir.path().join("credentials");
        setup_credential_store(dir.path(), &cred_file);

        let mirror = "https://collapse.invalid/mESmaC/rosen.git";
        let state = "https://api.collapse.invalid/mESmaC/rosen/rosen/state";
        let ledgers = "https://api.collapse.invalid/mESmaC/rosen/ledgers";
        std::fs::write(
            &cred_file,
            "https://rosen:SECRET_MIRROR_TOK@collapse.invalid/mESmaC/rosen.git\n\
             https://rosen:SECRET_STATE_TOK@api.collapse.invalid/mESmaC/rosen/rosen/state\n\
             https://rosen:SECRET_LEDGER_TOK@api.collapse.invalid/mESmaC/rosen/ledgers\n",
        )
        .unwrap();

        let (m, _) =
            check_credential("credential_mirror", Some(mirror), "mirror", dir.path()).await;
        let (s, _) = check_credential("credential_state", Some(state), "state", dir.path()).await;
        let (l, _) =
            check_credential("credential_ledgers", Some(ledgers), "ledgers", dir.path()).await;
        for item in [&m, &s, &l] {
            assert_eq!(item.status, CollapseCheckStatus::Pass, "{item:?}");
            let rendered = format!("{item:?}");
            for secret in ["SECRET_MIRROR_TOK", "SECRET_STATE_TOK", "SECRET_LEDGER_TOK"] {
                assert!(!rendered.contains(secret), "leaked token in {rendered}");
            }
        }

        std::fs::write(
            &cred_file,
            "https://rosen:SECRET_MIRROR_TOK@collapse.invalid/mESmaC/rosen.git\n\
             https://rosen:SECRET_LEDGER_TOK@api.collapse.invalid/mESmaC/rosen/ledgers\n",
        )
        .unwrap();
        let (m2, _) =
            check_credential("credential_mirror", Some(mirror), "mirror", dir.path()).await;
        let (s2, _) = check_credential("credential_state", Some(state), "state", dir.path()).await;
        let (l2, _) =
            check_credential("credential_ledgers", Some(ledgers), "ledgers", dir.path()).await;
        assert_eq!(m2.status, CollapseCheckStatus::Pass);
        assert_eq!(l2.status, CollapseCheckStatus::Pass);
        assert_eq!(s2.status, CollapseCheckStatus::Fail);
        assert!(
            s2.hint
                .as_deref()
                .is_some_and(|h| h.contains("git credential approve") && h.contains("/rosen/state")),
            "{s2:?}"
        );
    }
}
