//! The machine's run queue (SPEC R3 "Run queue").

use rosen_client::run_queue::{RunClass, RunQueue};

// Risk: the queue's order and slot count are the whole point of it. If a
// land's checks waited behind a warm pool prepare, or a later build passed an
// earlier one, or more builds ran than the class has slots, or a crashed
// waiter's ticket stayed in line, every agent on the machine would stall
// behind work that matters less, or all run at once and finish late together.
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

    queue.set_slots(RunClass::Build, Some(2)).unwrap();
    assert_eq!(queue.slots(RunClass::Build), 2, "the machine's own count");
    let build = queue.wait(RunClass::Build, "build").unwrap();
    let verify = queue.wait(RunClass::Verify, "verify").unwrap();
    let later_build = queue.wait(RunClass::Build, "later build").unwrap();
    let latest_build = queue.wait(RunClass::Build, "latest build").unwrap();
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
        latest_build.try_start().unwrap().is_none(),
        "a later build does not take a slot earlier ones are owed"
    );
    let building = build.try_start().unwrap().expect("the first build's turn");
    drop(build);
    let also_building = later_build
        .try_start()
        .unwrap()
        .expect("a second build beside the first, in the second slot");
    drop(later_build);
    assert!(
        latest_build.try_start().unwrap().is_none(),
        "no more builds at once than the class has slots"
    );
    drop(building);
    let building = latest_build
        .try_start()
        .unwrap()
        .expect("the latest build, in the freed slot");
    drop(latest_build);
    assert!(
        verify.try_start().unwrap().is_none(),
        "the tests after a land wait while a build runs"
    );
    drop((building, also_building));
    let verifying = verify
        .try_start()
        .unwrap()
        .expect("verify, beside the prepare");
    drop((verifying, prepare));
    let _ = std::fs::remove_dir_all(&dir);
}
