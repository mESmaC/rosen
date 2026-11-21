//! End-to-end contract tests for `rosen collapse link` and `rosen collapse unlink`
//! (SPEC R1, R10, #437).
//!
//! Risk: a link command that accepts tokens in argv, leaks tokens into output,
//! breaks existing `rosen.toml` settings, skips any of the three credential
//! paths or proactive auth, exposes secret-handling commands to MCP, or wipes
//! credentials on `unlink` without `--forget-credentials` would silently break
//! Collapse links or expose credentials.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

const TEST_SECRET: &str = "tok_collapse_link_test_secret_98765";

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

fn init_source_repo(repo: &Path, gitconfig: &Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", gitconfig)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
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

fn spawn_server(bin: &Path, data_dir: &Path, endpoint: &str, gitconfig: &Path) -> Child {
    let stderr_path = data_dir.with_file_name("server.stderr");
    let stderr_file = fs::File::create(&stderr_path).expect("create server stderr log");
    let mut child = Command::new(bin)
        .args(["--data-dir"])
        .arg(data_dir)
        .args(["--endpoint", endpoint, "--mirror-owner"])
        .env("GIT_CONFIG_GLOBAL", gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn rosen-server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
            panic!("rosen-server exited early ({status}) on {endpoint:?}: {stderr}");
        }
        let probe = Command::new(rosen_bin())
            .args(["--endpoint", endpoint, "server", "status"])
            .output()
            .unwrap();
        if probe.status.success() {
            return child;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    panic!("rosen-server did not start within 10s on {endpoint:?}: {stderr}");
}

fn run_rosen(
    endpoint: &str,
    gitconfig: &Path,
    args: &[&str],
    stdin_data: Option<&str>,
    token_env: Option<&str>,
) -> Output {
    let mut cmd = Command::new(rosen_bin());
    cmd.args(["--endpoint", endpoint])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("ROSEN_AGENT", "operator")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match token_env {
        Some(token) => {
            cmd.env("ROSEN_COLLAPSE_TOKEN", token);
        }
        None => {
            cmd.env_remove("ROSEN_COLLAPSE_TOKEN");
        }
    }
    if let Some(data) = stdin_data {
        let mut child = cmd.stdin(Stdio::piped()).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(data.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    } else {
        cmd.stdin(Stdio::null()).output().unwrap()
    }
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

    let mut child = spawn_server(&server, &data_dir, &endpoint, &gitconfig);
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

    let _ = run_rosen(&endpoint, &gitconfig, &["server", "stop"], None, None);
    let _ = child.wait();
    let _ = fs::remove_dir_all(&root);
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
