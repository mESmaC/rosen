//! End-to-end contract tests for `rosen collapse link` and `rosen collapse unlink`
//! (SPEC R1, R10, #437).
//!
//! Risk: a link command that accepts tokens in argv, leaks tokens into output,
//! breaks existing `rosen.toml` settings, skips any of the three credential
//! paths or proactive auth, exposes secret-handling commands to MCP, or wipes
//! credentials on `unlink` without `--forget-credentials` would silently break
//! Collapse links or expose credentials.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

const TEST_SECRET: &str = "tok_collapse_link_test_secret_98765";
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

fn rosen_bin() -> &'static str {
    env!("CARGO_BIN_EXE_rosen")
}

fn server_bin() -> Option<PathBuf> {
    let name = format!("rosen-server{}", std::env::consts::EXE_SUFFIX);
    let candidate = Path::new(rosen_bin()).parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

fn unique_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 0x10000;
    // Keep the path short so Unix domain sockets inside it stay under macOS's
    // 104-byte `sun_path` limit even under `/var/folders/.../T/`.
    let dir = std::env::temp_dir().join(format!(
        "rc-{label}-{}-{}-{nonce:04x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst),
    ));
    fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

fn endpoint_for(dir: &Path, label: &str) -> String {
    #[cfg(windows)]
    {
        let _ = dir;
        format!(r"\\.\pipe\rosen-{label}-{}", std::process::id())
    }
    #[cfg(not(windows))]
    {
        let _ = label;
        dir.join("s.sock").to_string_lossy().into_owned()
    }
}

fn setup_git_config(dir: &Path) -> (PathBuf, PathBuf) {
    let gitconfig = dir.join("gitconfig");
    let cred_file = dir.join("credentials");
    let cred_arg = cred_file.to_string_lossy().replace('\\', "/");
    fs::write(
        &gitconfig,
        format!(
            "[credential]\n\thelper =\n\thelper = store --file=\"{cred_arg}\"\n\tuseHttpPath = true\n"
        ),
    )
    .unwrap();
    (gitconfig, cred_file)
}

fn apply_test_git_env(cmd: &mut Command, gitconfig: &Path) {
    cmd.env("GIT_CONFIG_GLOBAL", gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT");
}

fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut reader) = pipe {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    } else {
        let _ = tx.send(Vec::new());
    }
    rx
}

fn wait_with_timeout(mut child: Child, timeout: Duration, label: &str) -> Result<Output, String> {
    let stdout_rx = drain_pipe(child.stdout.take());
    let stderr_rx = drain_pipe(child.stderr.take());
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|err| format!("{label}: wait failed: {err}"))?
        {
            let stdout = stdout_rx
                .recv_timeout(PIPE_DRAIN_TIMEOUT)
                .unwrap_or_default();
            let stderr = stderr_rx
                .recv_timeout(PIPE_DRAIN_TIMEOUT)
                .unwrap_or_default();
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stdout = stdout_rx
                .recv_timeout(PIPE_DRAIN_TIMEOUT)
                .unwrap_or_default();
            let stderr = stderr_rx
                .recv_timeout(PIPE_DRAIN_TIMEOUT)
                .unwrap_or_default();
            return Err(format!(
                "{label} timed out after {timeout:?} (stdout: {:?}, stderr: {:?})",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn run_command_with_timeout(
    mut cmd: Command,
    stdin_data: Option<&str>,
    timeout: Duration,
    label: &str,
) -> Result<Output, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = if stdin_data.is_some() {
        cmd.stdin(Stdio::piped())
    } else {
        cmd.stdin(Stdio::null())
    }
    .spawn()
    .map_err(|err| format!("{label}: spawn failed: {err}"))?;
    if let Some(data) = stdin_data {
        let payload = data.as_bytes().to_vec();
        let mut stdin = child.stdin.take().expect("stdin is piped");
        std::thread::spawn(move || {
            let _ = stdin.write_all(&payload);
        });
    }
    wait_with_timeout(child, timeout, label)
}

fn init_source_repo(repo: &Path, gitconfig: &Path) {
    let git = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com");
        apply_test_git_env(&mut cmd, gitconfig);
        let label = format!("git {args:?}");
        let out = run_command_with_timeout(cmd, None, GIT_TIMEOUT, &label)
            .unwrap_or_else(|err| panic!("{err}"));
        assert!(out.status.success(), "{label}: {out:?}");
    };
    fs::create_dir_all(repo).unwrap();
    git(&["init", "--quiet", "-b", "main"]);
    fs::write(
        repo.join("rosen.toml"),
        "# Existing project settings\nreview = \"after-land\"\n",
    )
    .unwrap();
    git(&["add", "rosen.toml"]);
    git(&["commit", "--quiet", "-m", "initial"]);
}

struct ServerProcess {
    child: Option<Child>,
    stderr_path: PathBuf,
}

impl ServerProcess {
    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn stop_and_wait(
        &mut self,
        endpoint: &str,
        gitconfig: &Path,
        timeout: Duration,
    ) -> Result<(), String> {
        let stop_out = run_rosen_with_timeout(
            endpoint,
            gitconfig,
            &["server", "stop"],
            None,
            None,
            timeout,
        )
        .map_err(|err| format!("{err}; server stderr: {}", self.stderr()))?;
        if !stop_out.status.success() {
            return Err(format!(
                "rosen server stop failed ({stop_out:?}); server stderr: {}",
                self.stderr()
            ));
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self
                .try_wait()
                .map_err(|err| format!("wait server exit: {err}"))?
                .is_some()
            {
                self.child = None;
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.kill_and_reap();
        Err(format!(
            "rosen-server child did not exit within {timeout:?} after stop; stderr: {}",
            self.stderr()
        ))
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn wait_for_server_ready(
    server: &mut ServerProcess,
    endpoint: &str,
    gitconfig: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = server
            .try_wait()
            .map_err(|err| format!("poll server: {err}"))?
        {
            return Err(format!(
                "rosen-server exited early ({status}) on {endpoint:?}: {}",
                server.stderr()
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let probe_timeout = remaining.min(PROBE_TIMEOUT);
        let mut probe_cmd = Command::new(rosen_bin());
        probe_cmd.args(["--endpoint", endpoint, "server", "status"]);
        apply_test_git_env(&mut probe_cmd, gitconfig);
        if let Ok(probe) =
            run_command_with_timeout(probe_cmd, None, probe_timeout, "server status probe")
        {
            if probe.status.success() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    server.kill_and_reap();
    Err(format!(
        "rosen-server did not start within {timeout:?} on {endpoint:?}: {}",
        server.stderr()
    ))
}

fn spawn_server(bin: &Path, data_dir: &Path, endpoint: &str, gitconfig: &Path) -> ServerProcess {
    let stderr_path = data_dir.with_file_name("server.stderr");
    let stderr_file = fs::File::create(&stderr_path).expect("create server stderr log");
    let mut cmd = Command::new(bin);
    cmd.args(["--data-dir"])
        .arg(data_dir)
        .args(["--endpoint", endpoint, "--mirror-owner"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));
    apply_test_git_env(&mut cmd, gitconfig);
    let child = cmd.spawn().expect("spawn rosen-server");
    let mut server = ServerProcess {
        child: Some(child),
        stderr_path,
    };
    wait_for_server_ready(&mut server, endpoint, gitconfig, SPAWN_TIMEOUT)
        .unwrap_or_else(|err| panic!("{err}"));
    server
}

fn run_rosen_with_timeout(
    endpoint: &str,
    gitconfig: &Path,
    args: &[&str],
    stdin_data: Option<&str>,
    token_env: Option<&str>,
    timeout: Duration,
) -> Result<Output, String> {
    let mut cmd = Command::new(rosen_bin());
    cmd.args(["--endpoint", endpoint])
        .args(args)
        .env("ROSEN_AGENT", "operator");
    apply_test_git_env(&mut cmd, gitconfig);
    match token_env {
        Some(token) => {
            cmd.env("ROSEN_COLLAPSE_TOKEN", token);
        }
        None => {
            cmd.env_remove("ROSEN_COLLAPSE_TOKEN");
        }
    }
    let label = format!("rosen {args:?}");
    run_command_with_timeout(cmd, stdin_data, timeout, &label)
}

fn run_rosen(
    endpoint: &str,
    gitconfig: &Path,
    args: &[&str],
    stdin_data: Option<&str>,
    token_env: Option<&str>,
) -> Output {
    run_rosen_with_timeout(
        endpoint,
        gitconfig,
        args,
        stdin_data,
        token_env,
        REQUEST_TIMEOUT,
    )
    .unwrap_or_else(|err| panic!("{err}"))
}

fn check_status(report: &Value, name: &str) -> String {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == name)
        .unwrap_or_else(|| panic!("missing check {name} in {report}"))["status"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn assert_no_secret_leak(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(TEST_SECRET),
        "token leaked on stdout: {stdout}"
    );
    assert!(
        !stderr.contains(TEST_SECRET),
        "token leaked on stderr: {stderr}"
    );
}

#[test]
fn collapse_link_validates_inputs_and_excludes_from_mcp() {
    let help = Command::new(rosen_bin())
        .args(["collapse", "link", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        !text.contains("--token"),
        "token must never be a CLI flag: {text}"
    );
    assert!(text.contains("ROSEN_COLLAPSE_TOKEN"), "{text}");

    // Bad arguments and missing token return rosen.error/v1 with --json.
    for args in [
        vec!["--json", "collapse", "link", "bad-slug"],
        vec![
            "--json",
            "collapse",
            "link",
            "mESmaC/example",
            "--host",
            "http://api.collapse.sh",
        ],
        vec![
            "--json",
            "collapse",
            "link",
            "mESmaC/example",
            "--relay",
            "bogus",
        ],
        vec!["--json", "collapse", "link", "mESmaC/example"],
    ] {
        let out = Command::new(rosen_bin())
            .args(&args)
            .env_remove("ROSEN_COLLAPSE_TOKEN")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stderr.is_empty(), "{args:?}: {:?}", out.stderr);
        let parsed: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(parsed["schema"], "rosen.error/v1");
    }

    // Operator-only `collapse link` and `collapse unlink` are never MCP tools;
    // read-only `collapse check` remains available.
    let mut mcp = Command::new(rosen_bin())
        .args(["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = mcp.stdin.take().unwrap();
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
        )
        .unwrap();
    }
    let out = mcp.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let replies: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"collapse_check"), "{names:?}");
    assert!(!names.contains(&"collapse_link"), "{names:?}");
    assert!(!names.contains(&"collapse_unlink"), "{names:?}");
}

#[test]
fn collapse_link_and_unlink_round_trip_with_live_server() {
    let Some(server) = server_bin() else {
        // Built by `cargo build --workspace` / `cargo test --workspace`.
        return;
    };
    let root = unique_dir("e2e");
    let data_dir = root.join("data");
    let source_repo = root.join("source");
    let endpoint = endpoint_for(&root, "collapse-link");
    let (gitconfig, cred_file) = setup_git_config(&root);
    init_source_repo(&source_repo, &gitconfig);

    let mut server = spawn_server(&server, &data_dir, &endpoint, &gitconfig);
    let import_out = run_rosen(
        &endpoint,
        &gitconfig,
        &["import", source_repo.to_str().unwrap()],
        None,
        None,
    );
    assert!(import_out.status.success(), "{import_out:?}");

    verify_link_with_stdin_and_land(&endpoint, &gitconfig, &cred_file);
    verify_unlink_preserves_creds_and_config(&endpoint, &gitconfig, &cred_file);
    verify_env_relink_and_forget_credentials(&endpoint, &gitconfig, &cred_file);

    server
        .stop_and_wait(&endpoint, &gitconfig, SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|err| panic!("server stop failed: {err}"));
    let _ = fs::remove_dir_all(&root);
}

/// Risk (#438): a deliberately broken server (one that never listens, or on
/// Unix accepts connections and stalls without answering) must fail fast
/// inside the test timeouts instead of hanging CI until the job limit.
#[test]
fn broken_server_fails_fast_within_timeout() {
    let root = unique_dir("broken");
    let endpoint = endpoint_for(&root, "collapse-broken");
    let (gitconfig, _) = setup_git_config(&root);
    let stderr_path = root.join("server.stderr");
    fs::write(&stderr_path, "").unwrap();

    // `rosen mcp` with a held-open stdin pipe stays alive without serving the
    // daemon endpoint, acting as a cross-platform deliberately broken server.
    let mut child = Command::new(rosen_bin())
        .args(["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hung dummy server");
    let _keep_stdin_open = child.stdin.take();
    let mut server = ServerProcess {
        child: Some(child),
        stderr_path,
    };

    let started = Instant::now();
    let err = wait_for_server_ready(&mut server, &endpoint, &gitconfig, Duration::from_secs(2))
        .expect_err("broken server must fail readiness check");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "readiness timeout took too long: {:?} ({err})",
        started.elapsed()
    );
    assert!(err.contains("did not start within"), "{err}");

    #[cfg(unix)]
    verify_wedged_socket_times_out(&endpoint, &gitconfig);

    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
fn verify_wedged_socket_times_out(endpoint: &str, gitconfig: &Path) {
    use std::os::unix::net::UnixListener;
    let listener = UnixListener::bind(endpoint).expect("bind silent unix socket");
    let (done_tx, done_rx) = mpsc::channel();
    let accept_thread = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = done_rx.recv_timeout(Duration::from_secs(10));
            drop(stream);
        }
    });
    let started = Instant::now();
    let err = run_rosen_with_timeout(
        endpoint,
        gitconfig,
        &["server", "status"],
        None,
        None,
        Duration::from_secs(2),
    )
    .expect_err("wedged socket must fail fast");
    let _ = done_tx.send(());
    let _ = accept_thread.join();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "request timeout took too long: {:?} ({err})",
        started.elapsed()
    );
    assert!(err.contains("timed out"), "{err}");
}

fn verify_link_with_stdin_and_land(endpoint: &str, gitconfig: &Path, cred_file: &Path) {
    // Using `.invalid` keeps the test offline; credential, config, repo_match
    // and relay checks all pass while network probes report unreachable host.
    let link_out = run_rosen(
        endpoint,
        gitconfig,
        &[
            "--json",
            "collapse",
            "link",
            "mESmaC/example",
            "--host",
            "https://collapse.invalid",
            "--relay",
            "approve,land,review-mark,config-change",
            "--land",
        ],
        Some(&format!("{TEST_SECRET}\n")),
        None,
    );
    assert_no_secret_leak(&link_out);
    let linked: Value = serde_json::from_slice(&link_out.stdout)
        .unwrap_or_else(|e| panic!("invalid JSON ({e}): {link_out:?}"));
    assert_eq!(linked["schema"], "rosen.collapse.link/v1");
    assert_eq!(
        linked["collapse"],
        "https://collapse.invalid/mESmaC/example"
    );
    assert_eq!(
        linked["mirror_remote"],
        "https://collapse.invalid/mESmaC/example.git"
    );
    assert_eq!(linked["proactive_auth_configured"], true);
    assert_eq!(linked["credentials_approved"].as_array().unwrap().len(), 3);
    assert_eq!(linked["land"]["outcome"]["status"], "landed");
    assert_eq!(
        linked["relay"]["allow"],
        serde_json::json!(["approve", "land", "mark", "config"])
    );
    for passed in [
        "mirror_owner",
        "collapse_config",
        "repo_match",
        "credential_mirror",
        "credential_state",
        "credential_ledgers",
        "relay_allow",
    ] {
        assert_eq!(
            check_status(&linked["check"], passed),
            "pass",
            "{passed}: {linked}"
        );
    }
    let cfg_text = fs::read_to_string(gitconfig).unwrap();
    assert!(
        cfg_text.contains("https://collapse.invalid/mESmaC/example.git")
            && cfg_text.contains("proactiveAuth = basic"),
        "{cfg_text}"
    );
    let stored_creds = fs::read_to_string(cred_file).unwrap();
    for path in [
        "/mESmaC/example.git",
        "/mESmaC/example/rosen/state",
        "/mESmaC/example/ledgers",
    ] {
        assert!(
            stored_creds.contains(path),
            "missing {path} in {stored_creds}"
        );
    }
}

fn verify_unlink_preserves_creds_and_config(endpoint: &str, gitconfig: &Path, cred_file: &Path) {
    let unlink_out = run_rosen(
        endpoint,
        gitconfig,
        &["--json", "collapse", "unlink", "--land"],
        None,
        None,
    );
    assert!(unlink_out.status.success(), "{unlink_out:?}");
    let unlinked: Value = serde_json::from_slice(&unlink_out.stdout).unwrap();
    assert_eq!(unlinked["schema"], "rosen.collapse.unlink/v1");
    assert_eq!(unlinked["ok"], true);
    assert_eq!(unlinked["land"]["outcome"]["status"], "landed");
    assert_eq!(unlinked["relay"]["allow"], serde_json::json!([]));
    assert_eq!(unlinked["proactive_auth_cleared"], true);
    assert_eq!(unlinked["credentials_forgotten"], serde_json::json!([]));
    let kept_creds = fs::read_to_string(cred_file).unwrap();
    assert!(
        kept_creds.contains("/mESmaC/example/rosen/state"),
        "{kept_creds}"
    );

    let config_out = run_rosen(
        endpoint,
        gitconfig,
        &["--json", "config", "show"],
        None,
        None,
    );
    assert!(config_out.status.success(), "{config_out:?}");
    let cfg: Value = serde_json::from_slice(&config_out.stdout).unwrap();
    assert_eq!(cfg["config"]["config"]["review"], "after-land");
    assert!(cfg["config"]["config"]["collapse"].is_null());
    assert!(cfg["config"]["config"]["mirror"]["remote"].is_null());
}

fn verify_env_relink_and_forget_credentials(endpoint: &str, gitconfig: &Path, cred_file: &Path) {
    // Re-link via ROSEN_COLLAPSE_TOKEN without --land leaves the change open and exits 0.
    let open_link = run_rosen(
        endpoint,
        gitconfig,
        &[
            "--json",
            "collapse",
            "link",
            "mESmaC/example",
            "--host",
            "https://collapse.invalid",
        ],
        None,
        Some(TEST_SECRET),
    );
    assert_no_secret_leak(&open_link);
    assert!(open_link.status.success(), "{open_link:?}");
    let open_val: Value = serde_json::from_slice(&open_link.stdout).unwrap();
    assert!(open_val["change"].is_object());
    assert!(open_val["land"].is_null());

    // Land a link, then verify --forget-credentials removes all three stored entries.
    let relink = run_rosen(
        endpoint,
        gitconfig,
        &[
            "--json",
            "collapse",
            "link",
            "mESmaC/example",
            "--host",
            "https://collapse.invalid",
            "--land",
        ],
        None,
        Some(TEST_SECRET),
    );
    assert_no_secret_leak(&relink);
    let forget = run_rosen(
        endpoint,
        gitconfig,
        &[
            "--json",
            "collapse",
            "unlink",
            "--land",
            "--forget-credentials",
        ],
        None,
        None,
    );
    assert!(forget.status.success(), "{forget:?}");
    let forgotten: Value = serde_json::from_slice(&forget.stdout).unwrap();
    assert_eq!(
        forgotten["credentials_forgotten"].as_array().unwrap().len(),
        3
    );
    let after_forget = fs::read_to_string(cred_file).unwrap_or_default();
    assert!(
        after_forget.trim().is_empty(),
        "credentials remained: {after_forget}"
    );
}
