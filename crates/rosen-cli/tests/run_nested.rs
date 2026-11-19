//! `rosen run` beneath another run (SPEC R3 "Run queue").

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rosen_client::run_queue::{RunClass, RunQueue, HELD_ENV, QUEUE_ENV};

// Risk: a script run through `rosen run` that queues its own steps with
// `rosen run` of the same class would wait on its own ancestor, which holds
// that class's turn, for ever. Every agent told to wrap a project's check
// script in `rosen run` would hang its turn and the machine's line with it.
#[test]
fn a_run_beneath_a_held_turn_runs_at_once() {
    let queue = std::env::temp_dir().join(format!("rosen-run-nested-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&queue);
    let ancestor = hold_every_build_slot(&queue);
    let rosen = env!("CARGO_BIN_EXE_rosen");
    let (shell, flag, done) = if cfg!(windows) {
        ("cmd", "/C", "exit 7")
    } else {
        ("sh", "-c", "exit 7")
    };
    let mut nested = Command::new(rosen)
        .args([
            "run", "--class", "build", "--", rosen, "run", "--class", "build", "--",
        ])
        .args([shell, flag, done])
        .env(QUEUE_ENV, &queue)
        .env(HELD_ENV, RunClass::Build.name())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = nested.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(60) {
            let _ = nested.kill();
            panic!("a nested run waited on the turn its own ancestor holds");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(ancestor);
    let _ = std::fs::remove_dir_all(&queue);
    assert_eq!(
        status.code(),
        Some(7),
        "the inner command's status comes back"
    );
}

/// Holds the build class's only slot, as the ancestor's turn would. The test
/// stands in for the ancestor because a real turn must also pass the memory
/// gate, which a runner with under 6 GiB available never does (#415).
fn hold_every_build_slot(queue: &Path) -> File {
    let line = RunQueue::at(queue);
    line.set_slots(RunClass::Build, Some(1)).unwrap();
    // Slot 0's lock keeps one name so older clients share the queue.
    let slot = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(queue.join("run-build.lock"))
        .unwrap();
    slot.lock().unwrap();
    let running = line.line().unwrap();
    assert!(
        running
            .iter()
            .any(|entry| entry.running && entry.class == RunClass::Build),
        "the queue sees the build slot held: {running:?}"
    );
    slot
}
