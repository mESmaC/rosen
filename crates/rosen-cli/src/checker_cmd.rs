//! `rosen checker run`: long-lived headless post-land checker (SPEC R12, R14, #440).
//!
//! Runs on an enrolled machine (or directly on the server) without Velocity.
//! Connects to the local daemon, reports its platform when requesting jobs,
//! executes commands through `rosen run` (defaulting to the `verify` class),
//! bisects failures, and posts results back to the server.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rosen_client::protocol::{
    BisectJob, BisectResult, BisectState, CheckKind, CheckPriority, EventKind, PostLandJob,
    RecordPostLandTiming, StepState,
};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::{AgentId, ObjectId, Platform, RecheckMode, ReviewMode};
use serde_json::json;
use tokio::io::AsyncReadExt;

use crate::commands::CheckerRunArgs;
use crate::server::Succeeded;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: usize = 64 * 1024;

struct CheckerConfig {
    platform: Platform,
    nick: Option<AgentId>,
    work_dir: PathBuf,
    json: bool,
    once: bool,
}

/// Runs the headless checker loop until Ctrl-C (or until idle when `--once`).
pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    args: &CheckerRunArgs,
) -> Result<Succeeded, Error> {
    let platform =
        Platform::resolve_override(args.platform.as_deref()).map_err(Error::InvalidArgument)?;
    let nick = resolve_nick(args.nick.as_deref())?;
    let work_dir = resolve_work_dir(args.work_dir.as_deref(), endpoint);
    let cfg = CheckerConfig {
        platform,
        nick,
        work_dir,
        json,
        once: args.once,
    };
    std::fs::create_dir_all(&cfg.work_dir)?;
    log_status(
        &cfg,
        "starting",
        json!({ "platform": cfg.platform.as_str(), "endpoint": endpoint.as_str() }),
    );
    let mut backoff = RECONNECT_MIN;
    loop {
        match run_session(endpoint, &cfg).await {
            Ok(()) => return Ok(true),
            Err(err) if cfg.once => return Err(err),
            Err(err) => {
                log_status(
                    &cfg,
                    "disconnected",
                    json!({ "error": err.to_string(), "retry_secs": backoff.as_secs() }),
                );
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    _ = tokio::signal::ctrl_c() => return Ok(true),
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

async fn run_session(endpoint: &Endpoint, cfg: &CheckerConfig) -> Result<(), Error> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    let mut events = client.subscribe(None).await?;
    log_status(
        cfg,
        "connected",
        json!({ "platform": cfg.platform.as_str(), "server": client.server_name() }),
    );
    loop {
        if !step_once(&client, cfg).await? {
            if cfg.once {
                return Ok(());
            }
            if !wait_for_work(&mut events).await {
                return Ok(());
            }
        }
    }
}

async fn wait_for_work(
    events: &mut tokio::sync::mpsc::Receiver<rosen_client::protocol::Event>,
) -> bool {
    let timer = tokio::time::sleep(POLL_INTERVAL);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return false,
            () = &mut timer => return true,
            event = events.recv() => match event {
                Some(ev) if matches!(ev.kind, EventKind::Landed | EventKind::Reverted) => return true,
                Some(_) => {}
                None => return true,
            },
        }
    }
}

/// Executes one pending bisection or post-land batch. Returns `true` if work
/// was performed, `false` when idle.
async fn step_once(client: &Client, cfg: &CheckerConfig) -> Result<bool, Error> {
    if resume_bisection(client, cfg).await? {
        return Ok(true);
    }
    let Some(job) = next_job(client, cfg).await? else {
        return Ok(false);
    };
    run_post_land_job(client, cfg, job).await?;
    Ok(true)
}

/// Resumes an open non-flaky bisection session matching this checker's platform.
async fn resume_bisection(client: &Client, cfg: &CheckerConfig) -> Result<bool, Error> {
    let status = client.post_land_status().await?;
    let session = status.sessions.into_iter().find(|s| {
        !s.flaky
            && s.steps.iter().any(|step| step.state == StepState::Offered)
            && session_matches(&cfg.platform, s.platform.as_deref())
    });
    let Some(session) = session else {
        return Ok(false);
    };
    let state = client.bisect_next(session.id).await?;
    drive_bisection(client, cfg, state).await?;
    Ok(true)
}

fn session_matches(checker: &Platform, session_platform: Option<&str>) -> bool {
    match session_platform {
        None => true,
        Some(raw) => Platform::parse(raw).is_ok_and(|target| checker.matches(&target)),
    }
}

async fn next_job(client: &Client, cfg: &CheckerConfig) -> Result<Option<PostLandJob>, Error> {
    let platform = cfg.platform.as_str();
    let wants_build = match client.project_config(None).await {
        Ok(record) => {
            record.config.review == ReviewMode::AfterLand
                || record.config.recheck == RecheckMode::Merge
        }
        Err(_) => false,
    };
    if wants_build {
        let build = client.post_land_build_next_for(platform).await?;
        if let Some(job) = build.job {
            return Ok(Some(job));
        }
    }
    let test = client.post_land_next_for(platform).await?;
    Ok(test.job)
}

async fn run_post_land_job(
    client: &Client,
    cfg: &CheckerConfig,
    mut job: PostLandJob,
) -> Result<(), Error> {
    log_status(
        cfg,
        "running",
        json!({
            "commit": job.commit,
            "kind": job.kind,
            "platform": job.platform,
            "commands": job.check_set.commands,
        }),
    );
    let (outcome, timings) = execute_commands(
        client,
        cfg,
        job.commit,
        job.kind,
        job.priority,
        &job.check_set.commands,
    )
    .await?;
    job.submitter = cfg.nick.clone();
    match outcome {
        Ok(()) => {
            let commit = job.commit;
            let kind = job.kind;
            let platform = job.platform.clone();
            client.post_land_passed(job).await?;
            record_post_land_timings(client, commit, kind, platform.clone(), None, &timings).await;
            log_status(
                cfg,
                "passed",
                json!({ "commit": commit, "kind": kind, "platform": platform }),
            );
        }
        Err(output) => {
            let commit = job.commit;
            let kind = job.kind;
            let platform = job.platform.clone();
            log_status(
                cfg,
                "failed",
                json!({ "commit": commit, "kind": kind, "platform": platform }),
            );
            let state = client.post_land_failed(job, output).await?;
            record_post_land_timings(client, commit, kind, platform, Some(state.id), &timings)
                .await;
            drive_bisection(client, cfg, state).await?;
        }
    }
    Ok(())
}

async fn drive_bisection(
    client: &Client,
    cfg: &CheckerConfig,
    mut state: BisectState,
) -> Result<(), Error> {
    while let Some(job) = state.job.clone() {
        let commands = bisect_commands(client, &cfg.platform, &job).await?;
        let priority = match job.kind {
            CheckKind::Build => CheckPriority::Landing,
            CheckKind::Test => CheckPriority::Verify,
        };
        log_status(
            cfg,
            "bisect_step",
            json!({
                "session": job.session,
                "step": job.step,
                "commit": job.commit,
                "kind": job.kind,
                "platform": job.platform,
            }),
        );
        let (outcome, timings) =
            execute_commands(client, cfg, job.commit, job.kind, priority, &commands).await?;
        let (passed, output) = match outcome {
            Ok(()) => (true, String::new()),
            Err(output) => (false, output),
        };
        let session = job.session;
        let commit = job.commit;
        let kind = job.kind;
        let platform = job.platform.clone();
        let result = BisectResult {
            job,
            passed,
            output,
            submitter: cfg.nick.clone(),
        };
        state = client.bisect_submit(result).await?;
        record_post_land_timings(client, commit, kind, platform, Some(session), &timings).await;
    }
    if let Some(culprit) = &state.culprit {
        let mut detail = json!({
            "session": state.id,
            "kind": state.kind,
            "platform": state.platform,
            "commit": culprit.commit,
            "change": culprit.change,
        });
        if !state.failing_tests.is_empty() {
            detail["failing_tests"] = json!(state.failing_tests);
        }
        log_status(cfg, "culprit", detail);
    } else if state.flaky {
        let mut detail =
            json!({ "session": state.id, "kind": state.kind, "platform": state.platform });
        if !state.failing_tests.is_empty() {
            detail["failing_tests"] = json!(state.failing_tests);
        }
        log_status(cfg, "flaky", detail);
    }
    Ok(())
}

async fn bisect_commands(
    client: &Client,
    checker_platform: &Platform,
    job: &BisectJob,
) -> Result<Vec<String>, Error> {
    let record = client.project_config(Some(job.commit)).await?;
    Ok(bisect_commands_for(
        &record.config,
        job.kind,
        checker_platform,
    ))
}

fn bisect_commands_for(
    config: &rosen_core::ProjectConfig,
    kind: CheckKind,
    checker_platform: &Platform,
) -> Vec<String> {
    let set = match kind {
        CheckKind::Build => &config.checks.pre_land,
        CheckKind::Test => &config.checks.post_land,
    };
    set.commands_for(Some(checker_platform))
}

#[derive(Default)]
struct CheckTimings {
    build_ms: Option<u64>,
    lint_ms: Option<u64>,
    test_ms: Option<u64>,
}

impl CheckTimings {
    fn record(&mut self, kind: CheckKind, command: &str, elapsed: Duration) {
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let lower = command.to_ascii_lowercase();
        let slot = if lower.contains("clippy")
            || lower.contains("fmt")
            || lower.contains("lint")
            || lower.contains("audit")
        {
            &mut self.lint_ms
        } else if kind == CheckKind::Build
            || lower.contains(" build")
            || lower.contains(" check")
            || lower.starts_with("lake build")
        {
            &mut self.build_ms
        } else {
            &mut self.test_ms
        };
        *slot = Some(slot.unwrap_or(0).saturating_add(ms));
    }
}

async fn record_post_land_timings(
    client: &Client,
    commit: ObjectId,
    kind: CheckKind,
    platform: Option<String>,
    bisect_session: Option<i64>,
    timings: &CheckTimings,
) {
    if timings.build_ms.is_none() && timings.lint_ms.is_none() && timings.test_ms.is_none() {
        return;
    }
    let _ = client
        .record_post_land_timing(RecordPostLandTiming {
            commit,
            kind,
            platform,
            bisect_session,
            build_ms: timings.build_ms,
            lint_ms: timings.lint_ms,
            test_ms: timings.test_ms,
        })
        .await;
}

async fn execute_commands(
    client: &Client,
    cfg: &CheckerConfig,
    commit: ObjectId,
    kind: CheckKind,
    priority: CheckPriority,
    commands: &[String],
) -> Result<(Result<(), String>, CheckTimings), Error> {
    let mut timings = CheckTimings::default();
    if commands.is_empty() {
        return Ok((Ok(()), timings));
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let checkout_dir = cfg
        .work_dir
        .join(format!("check-{}-{stamp}", &commit.to_hex()[..12]));
    let _cleanup = TempDirGuard(checkout_dir.clone());
    client.materialise_commit(commit, &checkout_dir).await?;
    let target_dir = cfg.work_dir.join("target");
    for command in commands {
        let started = Instant::now();
        let res = run_via_rosen_run(&checkout_dir, &target_dir, priority, command).await?;
        timings.record(kind, command, started.elapsed());
        if let Err(output) = res {
            return Ok((
                Err(format!("command `{command}` failed:\n{output}")),
                timings,
            ));
        }
    }
    Ok((Ok(()), timings))
}

/// Runs one command through `rosen run --class <class> --full -- <shell> ...`
/// so the checker respects this machine's run queue and resource budgets (R3).
async fn run_via_rosen_run(
    checkout: &Path,
    target_dir: &Path,
    priority: CheckPriority,
    command: &str,
) -> Result<Result<(), String>, Error> {
    let exe = std::env::current_exe()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.current_dir(checkout)
        .args(["run", "--class", priority.as_str(), "--full", "--"]);
    if cfg!(windows) {
        cmd.args(["cmd", "/C", command]);
    } else {
        cmd.args(["sh", "-c", command]);
    }
    if std::env::var_os("CARGO_TARGET_DIR").is_none() {
        cmd.env("CARGO_TARGET_DIR", target_dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let read_out = tokio::spawn(async move { read_tail(&mut stdout).await });
    let read_err = tokio::spawn(async move { read_tail_err(&mut stderr).await });
    let status = child.wait().await?;
    let out = read_out.await.unwrap_or_default();
    let err = read_err.await.unwrap_or_default();
    if status.success() {
        return Ok(Ok(()));
    }
    let combined = combine_output(&out, &err, &status.to_string());
    Ok(Err(combined))
}

async fn read_tail(pipe: &mut Option<tokio::process::ChildStdout>) -> Vec<u8> {
    let Some(reader) = pipe.as_mut() else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    keep_tail(buf)
}

async fn read_tail_err(pipe: &mut Option<tokio::process::ChildStderr>) -> Vec<u8> {
    let Some(reader) = pipe.as_mut() else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    keep_tail(buf)
}

fn keep_tail(bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() <= OUTPUT_LIMIT {
        bytes
    } else {
        bytes[bytes.len() - OUTPUT_LIMIT..].to_vec()
    }
}

fn combine_output(stdout: &[u8], stderr: &[u8], status: &str) -> String {
    let mut text = String::new();
    if !stdout.is_empty() {
        text.push_str(&String::from_utf8_lossy(stdout));
    }
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(stderr));
    }
    if text.trim().is_empty() {
        status.to_owned()
    } else {
        text
    }
}

fn resolve_nick(given: Option<&str>) -> Result<Option<AgentId>, Error> {
    let raw = given.map(str::to_owned).or_else(|| {
        std::env::var("ROSEN_AGENT")
            .ok()
            .filter(|v| !v.trim().is_empty())
    });
    let Some(raw) = raw else {
        return Ok(Some(AgentId::new("checker").expect("valid default nick")));
    };
    AgentId::new(raw.trim())
        .map(Some)
        .map_err(|err| Error::InvalidArgument(err.to_string()))
}

fn resolve_work_dir(given: Option<&Path>, endpoint: &Endpoint) -> PathBuf {
    if let Some(dir) = given {
        return dir.to_path_buf();
    }
    let key = ObjectId::hash_object("checker-work", endpoint.as_str().as_bytes());
    std::env::temp_dir().join(format!("rosen-checker-{}", &key.to_hex()[..12]))
}

fn log_status(cfg: &CheckerConfig, event: &str, detail: serde_json::Value) {
    if cfg.json {
        println!(
            "{}",
            json!({
                "schema": "rosen.checker/v1",
                "event": event,
                "detail": detail,
            })
        );
    } else {
        eprintln!("rosen checker: {event} {detail}");
    }
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Risk (R12, #460): bisection on a generic target must expand commands
    /// against the checker platform so arch-specific checks are not omitted.
    #[test]
    fn bisect_commands_use_checker_platform_for_generic_target() {
        let cfg = rosen_core::ProjectConfig::parse(
            "[checks.post_land]\nplatforms = [\"macos\"]\n\
             [checks.post_land.platform_commands]\nmacos-arm64 = [\"echo arm check\"]\n",
        )
        .unwrap();
        let arm = Platform::parse("macos-arm64").unwrap();
        let x64 = Platform::parse("macos-x86_64").unwrap();
        assert_eq!(
            bisect_commands_for(&cfg, CheckKind::Test, &arm),
            vec!["echo arm check"]
        );
        assert!(bisect_commands_for(&cfg, CheckKind::Test, &x64).is_empty());
    }
}
