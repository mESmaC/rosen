//! Read actual remote reachability; the stored pushed cursor can lag a
//! successful push if the daemon stopped before recording its acknowledgement.
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use rosen_core::ObjectId;

use crate::import::git_command;
use crate::ServerConfig;

pub fn name(config: &ServerConfig, remote: &str) -> Result<String> {
    if !Path::new(remote).is_absolute() && !remote.contains(':') {
        return Ok(std::path::absolute(config.mirror_staging_dir())?
            .join(remote)
            .to_string_lossy()
            .into_owned());
    }
    Ok(remote.to_owned())
}

pub struct Remote {
    _staging: tempfile::TempDir,
    refs: String,
    pub commits: HashSet<ObjectId>,
}

impl Remote {
    pub async fn inspect(config: &ServerConfig, remote: &str) -> Result<Self> {
        let staging = tempfile::tempdir_in(&config.data_dir)?;
        super::super::push::prepare_staging(staging.path(), &config.objects_dir()).await?;
        let refs = listing(staging.path(), remote).await?;
        if !refs.is_empty() {
            let mut args = vec![
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--",
                remote,
                "+refs/*:refs/rosen-remote/*",
            ];
            if refs.lines().any(|line| line.ends_with("\tHEAD")) {
                args.push("+HEAD:refs/rosen-remote-head");
            }
            run(staging.path(), remote, &args).await?;
        }
        verify_fetched(staging.path(), remote, &refs).await?;
        let history = run(staging.path(), remote, &["rev-list", "--all"]).await?;
        let commits = history
            .lines()
            .map(ObjectId::from_hex)
            .collect::<Result<_, _>>()?;
        let found = Self {
            _staging: staging,
            refs,
            commits,
        };
        found.unchanged(remote).await?;
        Ok(found)
    }

    pub async fn unchanged(&self, remote: &str) -> Result<()> {
        if listing(self._staging.path(), remote).await? != self.refs {
            bail!("mirror refs changed during repair; stop other writers to the mirror and retry");
        }
        Ok(())
    }
}

async fn verify_fetched(staging: &Path, remote: &str, refs: &str) -> Result<()> {
    let fetched = run(staging, remote, &["for-each-ref", "--format=%(objectname)"]).await?;
    let tips: HashSet<_> = fetched.lines().collect();
    for line in refs.lines().filter(|line| !line.ends_with("^{}")) {
        let (id, _) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("invalid mirror ref advertisement"))?;
        if !tips.contains(id) {
            bail!("mirror refs changed while fetching; stop other writers to the mirror and retry");
        }
    }
    Ok(())
}

async fn listing(staging: &Path, remote: &str) -> Result<String> {
    let output = run(staging, remote, &["ls-remote", "--", remote]).await?;
    let mut lines: Vec<_> = output.lines().collect();
    lines.sort_unstable();
    Ok(lines.join("\n"))
}

async fn run(staging: &Path, remote: &str, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(run_bytes(staging, remote, args).await?)?)
}

pub(in crate::mirror) async fn run_bytes(
    staging: &Path,
    remote: &str,
    args: &[&str],
) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::from(git_command(staging));
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(120), command.output()).await??;
    if !output.status.success() {
        bail!("cannot verify mirror history: {}. Restore mirror access and retry; no history was moved",
            super::super::hide_credentials(remote, &String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}
