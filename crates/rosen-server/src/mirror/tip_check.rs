//! The check before every mirror push (SPEC R1, #401): the remote's tip
//! must be the commit this server last pushed, or another commit the base
//! already holds, such as one a crash kept the server from recording.
//!
//! Anything else means another writer pushed there. Git would refuse the
//! push as `[rejected] (fetch first)`; this refuses first, naming both
//! commits and the commits the remote has that the base lacks, so a person
//! can tell whose lands they are and what to do.

use std::path::Path;
use std::process::Stdio;

use rosen_core::ObjectId;

use super::catch_up::{count_between, listing};
use super::push::{hide_credentials, prepare_staging, PushFailure};
use super::rewrite::remote::{name, run_bytes};
use super::MirrorTarget;
use crate::import::git_command;
use crate::ServerConfig;

/// What the remote's branch holds, when the push may go ahead.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RemoteTip {
    /// The branch is missing or behind the base: push.
    Behind,
    /// The branch already holds the base: a push accepted before a crash
    /// recorded it. Record it and push nothing.
    AtBase,
}

/// Reads the remote's `branch` and refuses when it holds commits `base`
/// lacks.
pub(super) async fn check(
    config: &ServerConfig,
    target: &MirrorTarget,
    branch: &str,
    base: &ObjectId,
) -> Result<RemoteTip, PushFailure> {
    let remote = target.remote.as_str();
    let fail = |message: String| PushFailure {
        rejected: false,
        message: hide_credentials(remote, &message),
    };
    // Git runs here as the push does, so a relative remote resolves alike.
    let staging = std::path::absolute(config.mirror_staging_dir())
        .map_err(|error| fail(format!("staging path: {error}")))?;
    prepare_staging(&staging, &config.objects_dir())
        .await
        .map_err(|error| fail(format!("preparing {}: {error}", staging.display())))?;
    let Some(tip) = remote_tip(&staging, remote, branch)
        .await
        .map_err(|error| fail(error.to_string()))?
    else {
        return Ok(RemoteTip::Behind);
    };
    if tip == *base {
        return Ok(RemoteTip::AtBase);
    }
    if target.pushed == Some(tip) || is_ancestor(&staging, &tip, base).await {
        return Ok(RemoteTip::Behind);
    }
    Err(PushFailure {
        rejected: true,
        message: hide_credentials(remote, &refusal(config, target, branch, tip, base).await),
    })
}

/// The commit the remote's `refs/heads/<branch>` names, or `None` when the
/// remote has no such branch yet.
async fn remote_tip(
    staging: &Path,
    remote: &str,
    branch: &str,
) -> anyhow::Result<Option<ObjectId>> {
    let full = format!("refs/heads/{branch}");
    let output = run_bytes(staging, remote, &["ls-remote", "--", remote, &full]).await?;
    let text = String::from_utf8(output)?;
    for line in text.lines() {
        if let Some((id, name)) = line.split_once('\t') {
            if name == full {
                return Ok(Some(id.trim().parse()?));
            }
        }
    }
    Ok(None)
}

/// True when `tip` is in `base`'s history. A tip the store lacks makes git
/// fail, which counts as not an ancestor: the base cannot hold it.
async fn is_ancestor(staging: &Path, tip: &ObjectId, base: &ObjectId) -> bool {
    let mut command = tokio::process::Command::from(git_command(staging));
    command
        .args(["merge-base", "--is-ancestor", &tip.to_hex(), &base.to_hex()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.status().await.is_ok_and(|status| status.success())
}

/// The refusal: both commits, the remote's own commits and what to run.
async fn refusal(
    config: &ServerConfig,
    target: &MirrorTarget,
    branch: &str,
    tip: ObjectId,
    base: &ObjectId,
) -> String {
    let last = target.pushed.map_or_else(
        || "this server has not recorded a push to it".to_owned(),
        |pushed| format!("this server last pushed {pushed}"),
    );
    let remote_side = match remote_only(config, &target.remote, branch, tip, base).await {
        Ok(text) => text,
        Err(error) => format!("  (could not list them: {error})\n"),
    };
    format!(
        "mirror branch {branch} is at {tip}, but {last}: another writer pushed there. \
         Nothing was pushed; base {base} stays local.\n\
         Mirror tip {tip} has commits the base lacks:\n{remote_side}\
         If another Rosen server landed there, run `rosen mirror catch-up` on this server, \
         and stop that server from pushing. Otherwise land the mirror's edits in Rosen, \
         then run `rosen mirror adopt {tip}` on the server."
    )
}

/// Fetches the remote's branch into a scratch repository, so the shared
/// staging repository gains no objects, and lists what only it has.
async fn remote_only(
    config: &ServerConfig,
    stored_remote: &str,
    branch: &str,
    tip: ObjectId,
    base: &ObjectId,
) -> anyhow::Result<String> {
    let remote = name(config, stored_remote)?;
    let scratch = tempfile::tempdir_in(std::path::absolute(&config.data_dir)?)?;
    let repo = scratch.path();
    prepare_staging(repo, &config.objects_dir()).await?;
    let fetched =
        super::adopt::fetch_branch(repo, &remote, &format!("refs/heads/{branch}")).await?;
    let total = count_between(repo, &remote, *base, fetched).await?;
    let mut text = listing(repo, &remote, *base, fetched, total).await?;
    if fetched != tip {
        text.push_str(&format!(
            "  (the branch moved to {fetched} while this was read)\n"
        ));
    }
    Ok(text)
}
