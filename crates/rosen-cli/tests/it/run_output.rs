use std::fs;
use std::path::Path;
use std::process::Command;

// Risk (R3 run command and status): the log could lose one stream, or the
// summary could hide the last diagnostics or change the command's exit code.
#[test]
fn failed_run_keeps_both_streams_and_summarizes_the_last_40_lines() {
    let workspace = std::env::temp_dir().join(format!("rosen-run-output-{}", std::process::id()));
    let state = workspace.join(".rosen");
    let subdir = workspace.join("subdir");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&subdir).unwrap();
    fs::write(state.join("workspace.json"), "{}").unwrap();
    // rosen reports the canonical path; on macOS the temp folder is a symlink
    // (/var -> /private/var), so compare against the canonical state folder.
    // Windows canonical paths carry a \\?\ prefix rosen does not print.
    #[cfg(not(windows))]
    let state = fs::canonicalize(&state).unwrap();
    let shell = if cfg!(windows) {
        [
            "powershell.exe",
            "-NoProfile",
            "-Command",
            "1..45 | ForEach-Object { 'line-' + $_ }; [Console]::Error.WriteLine('stderr-marker'); exit 7",
        ]
        .as_slice()
    } else {
        [
            "sh",
            "-c",
            "i=1; while [ \"$i\" -le 45 ]; do echo line-$i; i=$((i+1)); done; echo stderr-marker >&2; exit 7",
        ]
        .as_slice()
    };
    let output = Command::new(env!("CARGO_BIN_EXE_rosen"))
        .args(["run", "--"])
        .args(shell)
        .current_dir(&subdir)
        .env("ROSEN_RUN_HELD", "build")
        .output()
        .unwrap();

    let summary = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<_> = summary.lines().collect();
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(lines.len(), 42, "{summary}");
    assert_eq!(lines[1], "line-7");
    assert_eq!(lines[39..41], ["line-45", "stderr-marker"]);
    let log = Path::new(lines[41].strip_prefix("Log: ").unwrap());
    assert!(log.starts_with(state.join("run-logs")), "{log:?}");
    let contents = fs::read_to_string(log).unwrap();
    let expected = (1..=45)
        .map(|number| format!("line-{number}"))
        .chain(["stderr-marker".to_owned()]);
    assert!(contents.lines().eq(expected));
    fs::remove_dir_all(workspace).unwrap();
}

fn run_fixture(tag: &str, fixture: &str, exit_code: i32) -> (std::process::Output, String) {
    let workspace =
        std::env::temp_dir().join(format!("rosen-run-output-{tag}-{}", std::process::id()));
    let state = workspace.join(".rosen");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("workspace.json"), "{}").unwrap();
    let payload = workspace.join("fixture.txt");
    fs::write(&payload, fixture).unwrap();
    let command = if cfg!(windows) {
        let escaped = payload.display().to_string().replace('\'', "''");
        vec![
            "powershell.exe".to_owned(),
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            format!(
                "[Console]::Out.Write((Get-Content -Raw -LiteralPath '{escaped}')); exit {exit_code}"
            ),
        ]
    } else {
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("cat \"$1\"; exit {exit_code}"),
            "sh".to_owned(),
            payload.to_string_lossy().into_owned(),
        ]
    };
    let output = Command::new(env!("CARGO_BIN_EXE_rosen"))
        .args(["run", "--"])
        .args(&command)
        .current_dir(&workspace)
        .env("ROSEN_RUN_HELD", "build")
        .output()
        .unwrap();
    let summary = String::from_utf8_lossy(&output.stdout).into_owned();
    let _ = fs::remove_dir_all(&workspace);
    (output, summary)
}

// Risk (R3 run command and status): live `--no-capture` test output streams
// before `failures:`, so starting selection at `failures:` drops the failure
// detail, while passing test binaries must stay out of the summary.
#[test]
fn failed_run_preserves_live_rust_test_output_for_failing_binaries() {
    let fixture = concat!(
        "running 1 test\n",
        "passing-binary-noise\n",
        "test ok_binary ... ok\n",
        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        "running 2 tests\n",
        "test ok_sibling ... ok\n",
        "live-failure-detail\n",
        "test fixture_test ... FAILED\n",
        "\nfailures:\n\n\nfailures:\n",
        "    fixture_test\n",
        "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
    );
    let (output, summary) = run_fixture("live", fixture, 101);
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    assert!(summary.contains("live-failure-detail"), "{summary}");
    assert!(summary.contains("    fixture_test"), "{summary}");
    assert!(summary.contains("test result: FAILED."), "{summary}");
    assert!(!summary.contains("passing-binary-noise"), "{summary}");
    assert!(!summary.contains("test ok_sibling ... ok"), "{summary}");
}

// Risk (R3 run command and status): test stdout containing `test result:`
// lines can end failure selection early, hiding diagnostics and the repeated
// failing-test list.
#[test]
fn failed_run_ignores_non_structural_test_result_lines_in_captured_output() {
    let fixture = concat!(
        "running 1 test\n",
        "test fixture_test ... FAILED\n",
        "\nfailures:\n\n",
        "---- fixture_test stdout ----\n",
        "test result: application phase one\n",
        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        "captured-failure-detail\n\n\n",
        "failures:\n",
        "    fixture_test\n",
        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
    );
    let (output, summary) = run_fixture("result-prefix", fixture, 101);
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    assert!(summary.contains("captured-failure-detail"), "{summary}");
    assert!(summary.contains("    fixture_test"), "{summary}");
    assert!(
        summary.matches("test result: FAILED.").count() == 2,
        "{summary}"
    );
}
