//! `rosen collapse check|link|unlink`: manage and verify a server's Collapse
//! link (SPEC R1, R10, #435, #437).

use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;

use rosen_client::protocol::{
    ChangeRecord, CollapseCheckReport, CollapseCheckStatus, CollapseResetReport, ErrorCode,
    LandAnswer, LandStatus, NewChange, ReadAnswer, ReadRequest, RelaySetting, SnapshotRequest,
};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::{AgentId, ProjectConfig, CONFIG_PATH};
use serde_json::json;

use crate::collapse_config::{self, CollapseTarget, ConfigEdit};
use crate::collapse_git;
use crate::commands::{CollapseLinkArgs, CollapseUnlinkArgs};
use crate::output::{COLLAPSE_CHECK_SCHEMA, COLLAPSE_LINK_SCHEMA, COLLAPSE_UNLINK_SCHEMA};
use crate::server::Succeeded;

/// Connects to the running daemon (never starts an empty one implicitly) and
/// runs the read-only Collapse link verification. Exits non-zero if any check
/// fails.
pub async fn check(json: bool, endpoint: &Endpoint) -> Result<Succeeded, Error> {
    let client = Client::connect(endpoint).await?;
    let report = client.collapse_check().await?;
    print_report(json, &report);
    Ok(report.ok)
}

/// Links a running Rosen server to Collapse in one command (#437, #442):
/// 1. Reads the push token from stdin or `ROSEN_COLLAPSE_TOKEN`, sets
///    `credential.<origin>.useHttpPath = true`, approves git credentials for
///    `.git`, `/rosen/state` and `/ledgers`, and sets
///    `http.<mirror>.proactiveAuth = basic`.
/// 2. Opens and names a change updating `rosen.toml` with `collapse` and
///    `[mirror].remote`, landing it if `--land` is given.
/// 3. Sets the relay allow-list.
/// 4. Runs `rosen collapse check` and prints its result.
pub async fn link(
    json: bool,
    endpoint: &Endpoint,
    args: &CollapseLinkArgs,
) -> Result<Succeeded, Error> {
    match args.repo.as_deref() {
        Some(repo) => link_repo(json, endpoint, args, repo).await,
        None if args.reset => reset_existing(json, endpoint).await,
        None => Err(Error::InvalidArgument(
            "missing `<owner>/<name>` repository (or pass `--reset` to reset an already-linked Collapse feed)"
                .to_owned(),
        )),
    }
}

async fn link_repo(
    json: bool,
    endpoint: &Endpoint,
    args: &CollapseLinkArgs,
    repo: &str,
) -> Result<Succeeded, Error> {
    let target = CollapseTarget::parse(repo, &args.host).map_err(Error::InvalidArgument)?;
    let relay_kinds = crate::server::relay_kinds(&args.relay).map_err(Error::InvalidArgument)?;
    let owner = crate::change_cmd::resolve_owner(args.owner.as_deref())?;
    let token = collapse_git::read_push_token()?;

    let client = Client::connect(endpoint).await?;
    let status = client.status().await?;
    let existing = read_base_config_text(&client).await?;
    let edit = collapse_config::set_collapse_and_mirror(
        existing.as_deref(),
        &target.collapse_root,
        &target.mirror_remote,
    )
    .map_err(Error::InvalidArgument)?;

    let git_dir = collapse_git::existing_git_dir(&status.data_dir);
    let use_http_path_configured =
        collapse_git::set_use_http_path(&git_dir, &target.origin).await?;
    let credentials_approved =
        collapse_git::approve_credentials(&git_dir, &target.credential_urls(), &token).await?;
    let proactive_auth_configured =
        collapse_git::set_proactive_auth(&git_dir, &target.mirror_remote).await?;
    let message = format!("Link Collapse repository {}", target.repo_slug());
    let (change, land) = apply_config_edit(&client, owner, edit, message, args.land).await?;
    let relay = client.relay_set(relay_kinds).await?;
    let land_failed = is_land_refused(land.as_ref());
    let reset = if args.reset && !land_failed {
        Some(client.collapse_reset().await?)
    } else {
        None
    };
    let unlanded_change = !args.land && change.is_some();
    let poll_doorbell = !land_failed && !unlanded_change && relay.active();
    let check = check_waiting_for_doorbell(&client, poll_doorbell).await?;
    let ok = !land_failed && (unlanded_change || check.ok);
    print_link(
        json,
        &LinkSummary {
            ok,
            repo: &target.repo_slug(),
            collapse: &target.collapse_root,
            mirror_remote: &target.mirror_remote,
            origin: Some(&target.origin),
            use_http_path_configured,
            credentials_approved: &credentials_approved,
            proactive_auth_configured,
            change: change.as_ref(),
            land: land.as_ref(),
            relay: &relay,
            reset: reset.as_ref(),
            check: &check,
        },
    );
    Ok(ok)
}

async fn reset_existing(json: bool, endpoint: &Endpoint) -> Result<Succeeded, Error> {
    let client = Client::connect(endpoint).await?;
    let reset = client.collapse_reset().await?;
    let status = client.status().await?;
    let relay = status.relay.unwrap_or_default();
    let check = check_waiting_for_doorbell(&client, relay.active()).await?;
    let repo = slug_from_collapse_root(&reset.collapse);
    let mirror_remote = check
        .mirror_remote
        .clone()
        .unwrap_or_else(|| format!("{}.git", reset.collapse.trim_end_matches('/')));
    print_link(
        json,
        &LinkSummary {
            ok: check.ok,
            repo: &repo,
            collapse: &reset.collapse,
            mirror_remote: &mirror_remote,
            origin: None,
            use_http_path_configured: false,
            credentials_approved: &[],
            proactive_auth_configured: false,
            change: None,
            land: None,
            relay: &relay,
            reset: Some(&reset),
            check: &check,
        },
    );
    Ok(check.ok)
}

fn slug_from_collapse_root(root: &str) -> String {
    let trimmed = root.trim_end_matches('/');
    let parts: Vec<&str> = trimmed.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        format!("{}/{}", parts[1], parts[0])
    } else {
        trimmed.to_owned()
    }
}

/// How long `rosen collapse link` briefly polls `collapse check` while waiting
/// for Collapse to publish `/rosen/doorbell` right after enabling the relay (#447).
const DOORBELL_POLL_TIMEOUT: Duration = Duration::from_secs(10);
const DOORBELL_POLL_INITIAL: Duration = Duration::from_millis(500);
const DOORBELL_POLL_MAX: Duration = Duration::from_secs(2);

fn doorbell_not_yet_published(report: &CollapseCheckReport) -> bool {
    report.ok
        && report.checks.iter().any(|item| {
            item.name == "relay_doorbell"
                && item.status == CollapseCheckStatus::Skipped
                && item.detail.contains("not yet published")
        })
}

async fn check_waiting_for_doorbell(
    client: &Client,
    poll: bool,
) -> Result<CollapseCheckReport, Error> {
    poll_doorbell_check(
        poll,
        DOORBELL_POLL_TIMEOUT,
        DOORBELL_POLL_INITIAL,
        DOORBELL_POLL_MAX,
        || client.collapse_check(),
    )
    .await
}

async fn poll_doorbell_check<F, Fut>(
    poll: bool,
    timeout: Duration,
    initial_delay: Duration,
    max_delay: Duration,
    mut check_once: F,
) -> Result<CollapseCheckReport, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<CollapseCheckReport, Error>>,
{
    let mut report = check_once().await?;
    if !poll || !doorbell_not_yet_published(&report) {
        return Ok(report);
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut delay = initial_delay;
    while doorbell_not_yet_published(&report) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(delay.min(remaining)).await;
        report = check_once().await?;
        delay = (delay * 2).min(max_delay);
    }
    Ok(report)
}

/// Reverses `rosen collapse link`: opens and names a change removing `collapse`
/// and `[mirror].remote` from `rosen.toml` (landing it with `--land`), turns
/// off the action relay, clears proactive auth, and leaves stored credentials
/// alone unless `--forget-credentials` is given.
pub async fn unlink(
    json: bool,
    endpoint: &Endpoint,
    args: &CollapseUnlinkArgs,
) -> Result<Succeeded, Error> {
    let owner = crate::change_cmd::resolve_owner(args.owner.as_deref())?;
    let client = Client::connect(endpoint).await?;
    let status = client.status().await?;
    let config_record = client.project_config(None).await?;
    let existing = read_base_config_text(&client).await?;
    let edit = collapse_config::remove_collapse_and_mirror(existing.as_deref())
        .map_err(Error::InvalidArgument)?;

    let (mirror_url, cred_urls) = configured_collapse_urls(&config_record.config);
    let (change, land) = apply_config_edit(
        &client,
        owner,
        edit,
        "Unlink Collapse repository".to_owned(),
        args.land,
    )
    .await?;
    let relay = client.relay_set(Vec::new()).await?;

    let git_dir = collapse_git::existing_git_dir(&status.data_dir);
    let proactive_auth_cleared = match mirror_url.as_deref() {
        Some(url) => collapse_git::unset_proactive_auth(&git_dir, url).await?,
        None => false,
    };
    let credentials_forgotten = if args.forget_credentials {
        let refs: Vec<&str> = cred_urls.iter().map(String::as_str).collect();
        collapse_git::reject_credentials(&git_dir, &refs).await?
    } else {
        Vec::new()
    };

    let ok = !is_land_refused(land.as_ref());
    print_unlink(
        json,
        &UnlinkSummary {
            ok,
            change: change.as_ref(),
            land: land.as_ref(),
            relay: &relay,
            proactive_auth_cleared,
            credentials_forgotten: &credentials_forgotten,
        },
    );
    Ok(ok)
}

async fn read_base_config_text(client: &Client) -> Result<Option<String>, Error> {
    let request = ReadRequest {
        at: None,
        path: CONFIG_PATH.to_owned(),
    };
    match client.read_file(request).await {
        Ok(ReadAnswer::File { data, .. }) => String::from_utf8(data).map(Some).map_err(|_| {
            Error::InvalidArgument(format!("{CONFIG_PATH} on the base is not valid UTF-8"))
        }),
        Ok(ReadAnswer::Directory { .. }) => Err(Error::InvalidArgument(format!(
            "{CONFIG_PATH} on the base is a directory"
        ))),
        Err(Error::Server(body)) if body.code == ErrorCode::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

async fn apply_config_edit(
    client: &Client,
    owner: AgentId,
    edit: ConfigEdit,
    message: String,
    land: bool,
) -> Result<(Option<ChangeRecord>, Option<LandAnswer>), Error> {
    let payload = match edit {
        ConfigEdit::Unchanged => return Ok((None, None)),
        ConfigEdit::Write(text) => Some(text.into_bytes()),
        ConfigEdit::Delete => None,
    };
    let created = client.change_new(NewChange { owner, on: None }).await?;
    let change_id = created.change.id.to_string();
    let written = client
        .workspace_write(&change_id, CONFIG_PATH, payload)
        .await?;
    let head = written.change.change.head;
    let named = client
        .snapshot(SnapshotRequest {
            change: change_id.clone(),
            tree: head,
            from: Some(head),
            message: Some(message),
        })
        .await?;
    let land_answer = if land {
        Some(client.land(&change_id).await?)
    } else {
        None
    };
    Ok((Some(named.change), land_answer))
}

fn configured_collapse_urls(config: &ProjectConfig) -> (Option<String>, Vec<String>) {
    let mirror = config.mirror.remote.clone().or_else(|| {
        config
            .collapse
            .as_deref()
            .map(|root| format!("{}.git", root.trim_end_matches('/')))
    });
    let mut urls = BTreeSet::new();
    for url in [
        mirror.as_deref(),
        config.state_feed_url().as_deref(),
        config.ledger_push_url().as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if collapse_git::is_https(url) {
            urls.insert(url.to_owned());
        }
    }
    (mirror, urls.into_iter().collect())
}

fn is_land_refused(land: Option<&LandAnswer>) -> bool {
    matches!(land.map(|a| &a.outcome), Some(LandStatus::Refused { .. }))
}

struct LinkSummary<'a> {
    ok: bool,
    repo: &'a str,
    collapse: &'a str,
    mirror_remote: &'a str,
    origin: Option<&'a str>,
    use_http_path_configured: bool,
    credentials_approved: &'a [String],
    proactive_auth_configured: bool,
    change: Option<&'a ChangeRecord>,
    land: Option<&'a LandAnswer>,
    relay: &'a RelaySetting,
    reset: Option<&'a CollapseResetReport>,
    check: &'a CollapseCheckReport,
}

struct UnlinkSummary<'a> {
    ok: bool,
    change: Option<&'a ChangeRecord>,
    land: Option<&'a LandAnswer>,
    relay: &'a RelaySetting,
    proactive_auth_cleared: bool,
    credentials_forgotten: &'a [String],
}

fn print_link(json: bool, summary: &LinkSummary<'_>) {
    if json {
        let body = json!({
            "schema": COLLAPSE_LINK_SCHEMA,
            "ok": summary.ok,
            "repo": summary.repo,
            "collapse": summary.collapse,
            "mirror_remote": summary.mirror_remote,
            "credentials_approved": summary.credentials_approved,
            "proactive_auth_configured": summary.proactive_auth_configured,
            "change": summary.change,
            "land": summary.land,
            "relay": summary.relay,
            "reset": summary.reset.is_some(),
            "feed_id": summary.reset.map(|r| r.feed_id.as_str()),
            "check": summary.check,
        });
        println!("{body}");
        return;
    }
    println!("collapse:    {}", summary.collapse);
    println!("mirror:      {}", summary.mirror_remote);
    if let Some(origin) = summary.origin.filter(|_| summary.use_http_path_configured) {
        println!("http-path:   credential.{origin}.useHttpPath = true");
    }
    if !summary.credentials_approved.is_empty() {
        println!(
            "credentials: approved {} path(s)",
            summary.credentials_approved.len()
        );
    }
    if summary.proactive_auth_configured {
        println!(
            "proactive:   http.{}.proactiveAuth = basic",
            summary.mirror_remote
        );
    }
    print_change_and_land(summary.change, summary.land);
    println!("relay:       {}", crate::server::relay_line(summary.relay));
    if let Some(reset) = summary.reset {
        println!(
            "reset:       cleared Collapse feed tables (feed_id {})",
            reset.feed_id
        );
    }
    println!();
    print_check_lines(summary.check);
}

fn print_unlink(json: bool, summary: &UnlinkSummary<'_>) {
    if json {
        let body = json!({
            "schema": COLLAPSE_UNLINK_SCHEMA,
            "ok": summary.ok,
            "change": summary.change,
            "land": summary.land,
            "relay": summary.relay,
            "proactive_auth_cleared": summary.proactive_auth_cleared,
            "credentials_forgotten": summary.credentials_forgotten,
        });
        println!("{body}");
        return;
    }
    print_change_and_land(summary.change, summary.land);
    println!("relay:       {}", crate::server::relay_line(summary.relay));
    if summary.proactive_auth_cleared {
        println!("proactive:   cleared");
    }
    if summary.credentials_forgotten.is_empty() {
        println!("credentials: left alone (pass --forget-credentials to remove them)");
    } else {
        println!(
            "credentials: forgot {} path(s)",
            summary.credentials_forgotten.len()
        );
    }
}

fn print_change_and_land(change: Option<&ChangeRecord>, land: Option<&LandAnswer>) {
    let Some(record) = change else {
        println!("rosen.toml:  already up to date");
        return;
    };
    let id = record.change.id;
    match land.map(|answer| (&answer.base_ref, &answer.outcome)) {
        None => println!("change:      {id} (named; land it to apply rosen.toml)"),
        Some((base_ref, LandStatus::Landed { commit, .. })) => {
            println!("change:      {id} (landed as {commit} on {base_ref})");
        }
        Some((_, LandStatus::AlreadyLanded { commit })) => {
            println!("change:      {id} (already landed as {commit})");
        }
        Some((
            _,
            LandStatus::Refused {
                reason, message, ..
            },
        )) => {
            println!("change:      {id} (land refused ({reason}): {message})");
        }
    }
}

fn print_report(json: bool, report: &CollapseCheckReport) {
    if json {
        let body = json!({
            "schema": COLLAPSE_CHECK_SCHEMA,
            "ok": report.ok,
            "mirror_remote": report.mirror_remote,
            "collapse": report.collapse,
            "ledger_endpoint": report.ledger_endpoint,
            "mirror_owner": report.mirror_owner,
            "mirror_proactive_auth": report.mirror_proactive_auth,
            "checks": report.checks,
        });
        println!("{body}");
        return;
    }
    print_check_lines(report);
}

fn print_check_lines(report: &CollapseCheckReport) {
    for item in &report.checks {
        let status = item.status.as_str();
        match &item.hint {
            Some(hint) => println!(
                "{status:<7}  {:<24} {} — hint: {hint}",
                item.name, item.detail
            ),
            None => println!("{status:<7}  {:<24} {}", item.name, item.detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosen_client::protocol::CollapseCheckItem;

    fn report(ok: bool, status: CollapseCheckStatus, detail: &str) -> CollapseCheckReport {
        CollapseCheckReport {
            ok,
            mirror_remote: None,
            collapse: Some("https://api.collapse.sh/o/r".into()),
            ledger_endpoint: None,
            mirror_owner: true,
            mirror_proactive_auth: false,
            checks: vec![CollapseCheckItem {
                name: "relay_doorbell".into(),
                status,
                detail: detail.into(),
                hint: None,
            }],
        }
    }

    // Risk (R1, #447): `collapse link --land` runs immediately after `relay_set`
    // before Collapse publishes `/rosen/doorbell`; it must poll briefly until
    // published and skip polling when the relay is off or another check failed.
    #[tokio::test]
    async fn link_polls_briefly_while_doorbell_is_not_yet_published() {
        let mut calls = 0usize;
        let out = poll_doorbell_check(
            true,
            Duration::from_millis(200),
            Duration::from_millis(10),
            Duration::from_millis(20),
            || {
                calls += 1;
                let res = if calls < 3 {
                    report(
                        true,
                        CollapseCheckStatus::Skipped,
                        "doorbell not yet published at https://api.collapse.sh/o/r/rosen/doorbell",
                    )
                } else {
                    report(true, CollapseCheckStatus::Pass, "reachable (generation 0)")
                };
                async move { Ok(res) }
            },
        )
        .await
        .unwrap();
        assert_eq!(calls, 3);
        assert!(out.ok);
        assert_eq!(out.checks[0].status, CollapseCheckStatus::Pass);

        let mut off_calls = 0usize;
        let off = poll_doorbell_check(
            true,
            Duration::from_millis(200),
            Duration::from_millis(10),
            Duration::from_millis(20),
            || {
                off_calls += 1;
                async move { Ok(report(true, CollapseCheckStatus::Skipped, "relay is off")) }
            },
        )
        .await
        .unwrap();
        assert_eq!(off_calls, 1);
        assert!(off.ok);
    }
}
