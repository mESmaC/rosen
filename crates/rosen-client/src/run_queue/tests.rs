//! The machine's run queue (SPEC R3 "Run queue").

use super::{MemoryHeadroom, RunBudget, RunClass, RunQueue, Turn, Waiter, GIB, MEMORY_FLOOR};

fn admit(waiter: &Waiter, cores: usize, available: u64) -> Option<Turn> {
    waiter
        .try_start_on(cores, || MemoryHeadroom {
            available: Some(available),
            reserve: Some(MEMORY_FLOOR),
        })
        .unwrap()
}

fn start(waiter: &Waiter) -> Option<Turn> {
    admit(waiter, 16, 32 * GIB)
}

// Risk: the queue's order and slot count are the whole point of it. If a
// land's checks waited behind a warm pool prepare, or a later build passed an
// earlier one, or more builds ran than the class has slots, or a crashed
// waiter's ticket stayed in line, every agent on the machine would stall
// behind work that matters less, or all run at once and finish late together.
#[test]
fn runs_take_turns_by_class_and_arrival() {
    let dir = tempfile::tempdir().unwrap();
    let queue = RunQueue::at(dir.path());

    // What a process that died while waiting leaves: a ticket nobody holds.
    let dead = dir.path().join(format!("0-{:030}-1-0.wait", 1));
    std::fs::write(&dead, b"").unwrap();

    let prepare = start(&queue.wait(RunClass::Prepare, "prepare").unwrap()).unwrap();
    let landing = start(&queue.wait(RunClass::Landing, "landing").unwrap())
        .expect("a landing run starts beside a prepare, and past a dead ticket");
    assert!(!dead.exists(), "the dead waiter's ticket is cleared");

    queue.set_slots(RunClass::Build, Some(2)).unwrap();
    assert_eq!(queue.slots(RunClass::Build), 2, "the machine's own count");
    let build = queue.wait(RunClass::Build, "build").unwrap();
    let verify = queue.wait(RunClass::Verify, "verify").unwrap();
    let later_build = queue.wait(RunClass::Build, "later build").unwrap();
    let latest_build = queue.wait(RunClass::Build, "latest build").unwrap();
    assert!(start(&build).is_none(), "a build waits for a landing run");
    drop(landing);
    assert!(
        start(&verify).is_none(),
        "the tests after a land wait while a build waits"
    );
    assert!(
        start(&latest_build).is_none(),
        "a later build does not take a slot earlier ones are owed"
    );
    let building = start(&build).expect("the first build's turn");
    drop(build);
    let also_building =
        start(&later_build).expect("a second build beside the first, in the second slot");
    drop(later_build);
    assert!(
        start(&latest_build).is_none(),
        "no more builds at once than the class has slots"
    );
    drop(building);
    let building = start(&latest_build).expect("the latest build, in the freed slot");
    drop(latest_build);
    assert!(
        start(&verify).is_none(),
        "the tests after a land wait while a build runs"
    );
    drop((building, also_building));
    let verifying = start(&verify).expect("verify, beside the prepare");
    drop((verifying, prepare));
}

// Risk: separate class slots can admit work against the same free memory or
// cores. A prepare turn blocks a landing turn if either budget is spent.
#[test]
fn every_class_charges_live_turns_before_starting() {
    let dir = tempfile::tempdir().unwrap();
    let queue = RunQueue::at(dir.path());
    let prepare = queue.wait(RunClass::Prepare, "prepare").unwrap();
    let landing = queue.wait(RunClass::Landing, "land").unwrap();
    assert!(admit(&landing, 4, MEMORY_FLOOR + 3 * GIB - 1).is_none());
    // Start the prepare before the landing waiter joins the line.
    drop(landing);
    let preparing = admit(&prepare, 4, 32 * GIB).unwrap();
    drop(prepare);

    let landing = queue.wait(RunClass::Landing, "land").unwrap();
    assert!(admit(&landing, 4, MEMORY_FLOOR + 6 * GIB - 1).is_none());
    assert!(admit(&landing, 3, 32 * GIB).is_some());
    assert!(admit(&landing, 2, 32 * GIB).is_none());
    assert!(admit(&landing, 4, MEMORY_FLOOR + 6 * GIB).is_some());

    drop(preparing);
    assert!(admit(&landing, 4, MEMORY_FLOOR + 3 * GIB).is_some());
}

// Risk: a light run charged at its class's full cap waits for memory it will
// never use (mESmaC/rosen#417); one left out of the queue's count lets a heavy
// turn be admitted on memory the light run is already using.
#[test]
fn a_caller_set_budget_is_admitted_and_counted_on_what_it_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let queue = RunQueue::at(dir.path());
    let verify = queue.wait(RunClass::Verify, "verify").unwrap();
    let _verifying = admit(&verify, 16, 32 * GIB).unwrap();
    drop(verify);

    let full = queue.wait(RunClass::Build, "full").unwrap();
    assert!(admit(&full, 16, MEMORY_FLOOR + 10 * GIB).is_none());
    drop(full);
    let light = queue
        .wait_with(RunClass::Build, RunBudget::new(2, 3), "light")
        .unwrap();
    let lighting = admit(&light, 16, MEMORY_FLOOR + 10 * GIB)
        .expect("a 2-job, 3 GiB turn fits beside a 6 GiB verify turn");
    assert_eq!(
        (lighting.budget().jobs, lighting.budget().memory),
        (2, 3 * GIB)
    );
    drop(light);
    assert_eq!(queue.running_budget().unwrap().memory, 9 * GIB);

    let landing = queue.wait(RunClass::Landing, "land").unwrap();
    assert!(admit(&landing, 16, MEMORY_FLOOR + 15 * GIB - 1).is_none());
    assert!(admit(&landing, 16, MEMORY_FLOOR + 15 * GIB).is_some());

    let greedy = queue
        .wait_with(RunClass::Prepare, RunBudget::new(9, 99), "greedy")
        .unwrap();
    let cap = RunClass::Prepare.budget_on(16);
    assert_eq!(
        (greedy.budget_on(16).jobs, greedy.budget_on(16).memory),
        (cap.jobs, cap.memory)
    );
}
