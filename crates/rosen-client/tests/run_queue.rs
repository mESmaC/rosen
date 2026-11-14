//! The machine's run queue (SPEC R3 "Run queue").

use rosen_client::run_queue::{RunClass, RunQueue};

// Risk: the queue's order is the whole point of it. If a land's checks waited
// behind a warm pool prepare, or a later build passed an earlier one, or a
// crashed waiter's ticket stayed in line, every agent on the machine would
// stall behind work that matters less or is not running at all.
#[test]
fn runs_take_turns_by_class_and_arrival() {
    let dir = std::env::temp_dir().join(format!("rosen-run-queue-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let queue = RunQueue::at(&dir);

    // What a process that died while waiting leaves: a ticket nobody holds.
    std::fs::create_dir_all(&dir).unwrap();
    let dead = dir.join(format!("0-{:030}-1-0.wait", 1));
    std::fs::write(&dead, b"").unwrap();

    let prepare = queue.turn(RunClass::Prepare, "prepare").unwrap();
    let landing = queue
        .wait(RunClass::Landing, "landing")
        .unwrap()
        .try_start()
        .unwrap()
        .expect("a landing run starts beside a prepare, and past a dead ticket");
    assert!(!dead.exists(), "the dead waiter's ticket is cleared");

    let build = queue.wait(RunClass::Build, "build").unwrap();
    let verify = queue.wait(RunClass::Verify, "verify").unwrap();
    let later_build = queue.wait(RunClass::Build, "later build").unwrap();
    assert!(
        build.try_start().unwrap().is_none(),
        "a build waits for a landing run"
    );
    drop(landing);
    assert!(
        verify.try_start().unwrap().is_none(),
        "the tests after a land wait while a build waits"
    );
    assert!(
        later_build.try_start().unwrap().is_none(),
        "a later build does not pass an earlier one"
    );
    let building = build.try_start().unwrap().expect("the first build's turn");
    drop(build);
    assert!(
        later_build.try_start().unwrap().is_none(),
        "one build at a time"
    );
    drop(building);
    let building = later_build.try_start().unwrap().expect("the later build");
    drop(later_build);
    assert!(
        verify.try_start().unwrap().is_none(),
        "the tests after a land wait while a build runs"
    );
    drop(building);
    let verifying = verify
        .try_start()
        .unwrap()
        .expect("verify, beside the prepare");
    drop((verifying, prepare));
    let _ = std::fs::remove_dir_all(&dir);
}
