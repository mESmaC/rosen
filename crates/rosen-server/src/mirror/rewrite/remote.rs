//! Read actual remote reachability; the stored pushed cursor can lag a
//! successful push if the daemon stopped before recording its acknowledgement.
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use anyhow::{bail, Result};
use rosen_core::ObjectId;

use super::super::proactive_auth::{self, ProactiveAuth};
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
    pub async fn inspect(
        config: &ServerConfig,
        remote: &str,
        auth: &ProactiveAuth,
    ) -> Result<Self> {
        let staging = tempfile::tempdir_in(&config.data_dir)?;
        super::super::push::prepare_staging(staging.path(), &config.objects_dir()).await?;
        let refs = listing(staging.path(), remote, auth).await?;
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
            run(staging.path(), remote, &args, auth).await?;
        }
        verify_fetched(staging.path(), remote, &refs, auth).await?;
        let history = run(staging.path(), remote, &["rev-list", "--all"], auth).await?;
        let commits = history
            .lines()
            .map(ObjectId::from_hex)
            .collect::<Result<_, _>>()?;
        let found = Self {
            _staging: staging,
            refs,
            commits,
        };
        found.unchanged(remote, auth).await?;
        Ok(found)
    }

    pub async fn unchanged(&self, remote: &str, auth: &ProactiveAuth) -> Result<()> {
        if listing(self._staging.path(), remote, auth).await? != self.refs {
            bail!("mirror refs changed during repair; stop other writers to the mirror and retry");
        }
        Ok(())
    }
}

async fn verify_fetched(
    staging: &Path,
    remote: &str,
    refs: &str,
    auth: &ProactiveAuth,
) -> Result<()> {
    let fetched = run(
        staging,
        remote,
        &["for-each-ref", "--format=%(objectname)"],
        auth,
    )
    .await?;
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

async fn listing(staging: &Path, remote: &str, auth: &ProactiveAuth) -> Result<String> {
    let output = run(staging, remote, &["ls-remote", "--", remote], auth).await?;
    let mut lines: Vec<_> = output.lines().collect();
    lines.sort_unstable();
    Ok(lines.join("\n"))
}

async fn run(staging: &Path, remote: &str, args: &[&str], auth: &ProactiveAuth) -> Result<String> {
    Ok(String::from_utf8(
        run_bytes(staging, remote, args, auth).await?,
    )?)
}

pub(in crate::mirror) async fn run_bytes(
    staging: &Path,
    remote: &str,
    args: &[&str],
    auth: &ProactiveAuth,
) -> Result<Vec<u8>> {
    run_bytes_with(
        remote,
        args,
        auth,
        |proactive| run_git(staging, args, proactive),
        proactive_auth::git_supports_proactive_auth,
    )
    .await
}

async fn run_git(staging: &Path, args: &[&str], proactive: bool) -> Result<Output> {
    let mut command = tokio::process::Command::from(git_command(staging));
    if proactive {
        command.args(proactive_auth::PROACTIVE_AUTH_ARGS);
    }
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("LC_ALL", "C")
        .kill_on_drop(true);
    Ok(tokio::time::timeout(Duration::from_secs(120), command.output()).await??)
}

async fn run_bytes_with<Exec, ExecFut, Check, CheckFut>(
    remote: &str,
    args: &[&str],
    auth: &ProactiveAuth,
    mut exec: Exec,
    check_git: Check,
) -> Result<Vec<u8>>
where
    Exec: FnMut(bool) -> ExecFut,
    ExecFut: Future<Output = Result<Output>>,
    Check: FnOnce() -> CheckFut,
    CheckFut: Future<Output = bool>,
{
    let https_read = proactive_auth::is_remote_read(args) && proactive_auth::is_https(remote);
    let proactive = https_read && auth.is_enabled();
    let output = exec(proactive).await?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if https_read && !proactive && proactive_auth::is_not_found(&stderr) {
        if !check_git().await {
            bail!("{}", proactive_auth::OLD_GIT_MESSAGE);
        }
        let retry = exec(true).await?;
        if retry.status.success() {
            auth.enable();
            return Ok(retry.stdout);
        }
        bail_history_error(remote, &String::from_utf8_lossy(&retry.stderr))?;
    }
    bail_history_error(remote, &stderr)
}

fn bail_history_error<T>(remote: &str, stderr: &str) -> Result<T> {
    bail!(
        "cannot verify mirror history: {}. Restore mirror access and retry; no history was moved",
        super::super::hide_credentials(remote, stderr)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn output(ok: bool, stdout: &[u8], stderr: &[u8]) -> Output {
        #[cfg(unix)]
        let status = std::os::unix::process::ExitStatusExt::from_raw(if ok { 0 } else { 128 << 8 });
        #[cfg(windows)]
        let status = std::os::windows::process::ExitStatusExt::from_raw(if ok { 0 } else { 128 });
        Output {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    // Risk (SPEC R1, #432): private HTTPS hosts answer unauthenticated reads
    // with 404, stalling pushes forever unless Rosen retries with proactive
    // basic auth on git >= 2.46, caches it for the server's lifetime, and
    // reports it in server status.
    #[tokio::test]
    async fn https_not_found_retries_with_proactive_auth_and_caches_for_server_lifetime() {
        let remote = "https://collapse.example/org/private.git";
        let args = ["ls-remote", "--", remote, "refs/heads/main"];
        let data = tempfile::tempdir().unwrap();
        let state = crate::ServerState::new(ServerConfig {
            data_dir: data.path().to_path_buf(),
            endpoint: rosen_protocol::Endpoint::new("rosen-proactive-auth-test-unused"),
            warm_pool_dir: None,
        });
        assert_eq!(status_proactive_auth(&state).await, Some(false));

        let calls = AtomicUsize::new(0);
        let first = run_bytes_with(
            remote,
            &args,
            &state.mirror_proactive_auth,
            |proactive| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if !proactive {
                        assert_eq!(n, 0);
                        Ok(output(
                            false,
                            b"",
                            b"remote: Repository not found.\nfatal: repository 'https://collapse.example/org/private.git/' not found\n",
                        ))
                    } else {
                        Ok(output(true, b"deadbeef\trefs/heads/main\n", b""))
                    }
                }
            },
            || async { true },
        )
        .await
        .expect("retry with proactive auth should succeed");
        assert_eq!(first, b"deadbeef\trefs/heads/main\n");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(status_proactive_auth(&state).await, Some(true));

        let second = run_bytes_with(
            remote,
            &args,
            &state.mirror_proactive_auth,
            |proactive| async move {
                assert!(proactive, "cached proactive auth must be used directly");
                Ok(output(true, b"cafebabe\trefs/heads/main\n", b""))
            },
            || async { panic!("git version check not needed once cached") },
        )
        .await
        .expect("second read should succeed");
        assert_eq!(second, b"cafebabe\trefs/heads/main\n");
    }

    async fn status_proactive_auth(state: &std::sync::Arc<crate::ServerState>) -> Option<bool> {
        let rosen_protocol::Outcome::Ok(rosen_protocol::Response::Status(status)) =
            crate::rpc::dispatch(state, rosen_protocol::Request::Status).await
        else {
            panic!("expected status response");
        };
        status.mirror_proactive_auth
    }

    // Risk (SPEC R1, #432): older git lacks http.proactiveAuth and must guide
    // the operator instead of retrying forever; non-HTTPS remotes never retry.
    #[tokio::test]
    async fn older_git_reports_upgrade_guidance_and_non_https_skips_retry() {
        let remote = "https://collapse.example/org/private.git";
        let args = ["ls-remote", "--", remote, "refs/heads/main"];
        let old_auth = ProactiveAuth::default();
        let old_err = run_bytes_with(
            remote,
            &args,
            &old_auth,
            |proactive| async move {
                assert!(!proactive);
                Ok(output(
                    false,
                    b"",
                    b"fatal: repository 'https://collapse.example/org/private.git/' not found\n",
                ))
            },
            || async { false },
        )
        .await
        .unwrap_err();
        assert_eq!(old_err.to_string(), proactive_auth::OLD_GIT_MESSAGE);
        assert!(!old_auth.is_enabled());

        let ssh_auth = ProactiveAuth::default();
        let ssh_err = run_bytes_with(
            "ssh://git@example.com/org/private.git",
            &["ls-remote", "--", "ssh://git@example.com/org/private.git"],
            &ssh_auth,
            |proactive| async move {
                assert!(!proactive);
                Ok(output(false, b"", b"remote: Repository not found.\n"))
            },
            || async { panic!("non-HTTPS must not check git version") },
        )
        .await
        .unwrap_err();
        assert!(ssh_err.to_string().contains("cannot verify mirror history"));
        assert!(!ssh_auth.is_enabled());
    }

    #[test]
    fn parses_git_versions_and_not_found_diagnostics() {
        assert_eq!(
            proactive_auth::parse_git_version("git version 2.46.0\n"),
            Some((2, 46))
        );
        assert_eq!(
            proactive_auth::parse_git_version("git version 2.47.1.windows.1\r\n"),
            Some((2, 47))
        );
        assert_eq!(
            proactive_auth::parse_git_version("git version 2.39.5 (Apple Git-154)\n"),
            Some((2, 39))
        );
        assert!(proactive_auth::version_supports_proactive_auth((2, 46)));
        assert!(!proactive_auth::version_supports_proactive_auth((2, 45)));
        assert!(proactive_auth::is_not_found(
            "fatal: repository 'https://h/r.git/' not found"
        ));
        assert!(proactive_auth::is_not_found(
            "remote: Repository not found."
        ));
        assert!(!proactive_auth::is_not_found(
            "fatal: Authentication failed for 'https://h/r.git/'"
        ));
    }
}
