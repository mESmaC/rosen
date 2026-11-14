//! `rosen run` beneath another run (SPEC R3 "Run queue").

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// Risk: a script run through `rosen run` that queues its own steps with
// `rosen run` of the same class would wait on its own ancestor, which holds
// that class's turn, for ever. Every agent told to wrap a project's check
// script in `rosen run` would hang its turn and the machine's line with it.
#[test]
fn a_run_beneath_a_held_turn_runs_at_once() {
    let queue = std::env::temp_dir().join(format!("rosen-run-nested-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&queue);
    let rosen = env!("CARGO_BIN_EXE_rosen");
    let (shell, flag, done) = if cfg!(windows) {
        ("cmd", "/C", "exit 7")
    } else {
        ("sh", "-c", "exit 7")
    };
    let mut outer = Command::new(rosen)
        .args([
            "run", "--class", "build", "--", rosen, "run", "--class", "build", "--",
        ])
        .args([shell, flag, done])
        .env("ROSEN_RUN_QUEUE", &queue)
        .env_remove("ROSEN_RUN_HELD")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = outer.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(60) {
            let _ = outer.kill();
            panic!("the inner run waited on the turn its own parent holds");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = std::fs::remove_dir_all(&queue);
    assert_eq!(
        status.code(),
        Some(7),
        "the inner command's status comes back"
    );
}
