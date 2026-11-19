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
