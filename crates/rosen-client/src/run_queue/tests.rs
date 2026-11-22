//! The machine's run queue (SPEC R3 "Run queue").

use std::time::Instant;

use super::{
    now_nanos, MemoryHeadroom, RunBudget, RunClass, RunQueue, Turn, Waiter, GIB, MEMORY_FLOOR,
    STARVING,
};

fn admit_with_reserve(waiter: &Waiter, cores: usize, available: u64, reserve: u64) -> Option<Turn> {
    waiter
        .try_start_on(cores, || MemoryHeadroom {
            available: Some(available),
            reserve: Some(reserve),
        })
        .unwrap()
}

fn admit(waiter: &Waiter, cores: usize, available: u64) -> Option<Turn> {
    admit_with_reserve(waiter, cores, available, MEMORY_FLOOR)
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

// Risk: a big turn blocked only on memory starves while smaller, later or
// less important turns keep filling the room it needs (mESmaC/rosen#421):
// the tests after a land then never run and every land queues behind them.
#[test]
fn a_waiter_blocked_on_memory_keeps_its_room_from_later_turns() {
    let dir = tempfile::tempdir().unwrap();
    let queue = RunQueue::at(dir.path());
    queue.set_slots(RunClass::Prepare, Some(2)).unwrap();
    let preparing = start(&queue.wait(RunClass::Prepare, "prepare").unwrap()).unwrap();
    // 3 GiB is held, so the 6 GiB verify turn needs 9 of the 8 GiB above the
    // reserve, and fits once the prepare ends.
    let available = MEMORY_FLOOR + 8 * GIB;
    let verify = queue.wait(RunClass::Verify, "verify").unwrap();
    assert!(admit(&verify, 16, available).is_none());

    // The incident's prepare: past its patience, so it no longer yields.
    let mut small = queue
        .wait_with(RunClass::Prepare, RunBudget::new(1, 1), "small prepare")
        .unwrap();
    small.since = Instant::now()
        .checked_sub(RunClass::Prepare.patience())
        .unwrap();
    assert!(
        admit(&small, 16, available).is_none(),
        "a later 1 GiB turn would push the verify turn out of reach"
    );
    let reason = small.waiting_for().unwrap();
    assert!(
        reason.contains("keeping room for verify (6.0 GiB)"),
        "{reason}"
    );

    drop(preparing);
    let verifying = admit(&verify, 16, available).expect("verify, once the prepare drains");
    drop(verify);
    let _small = admit(&small, 16, available).expect("the small turn fits beside verify");
    drop(verifying);
}

// Risk: a large RAM-based reserve stops the only queued run when available
// memory is below `reserve + budget` but above `MEMORY_FLOOR + budget`,
// deadlocking an idle machine (mESmaC/rosen#431).
#[test]
fn idle_head_waiter_uses_floor_while_concurrency_keeps_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let queue = RunQueue::at(dir.path());
    let reserve = 12 * GIB;
    let available = 14 * GIB;

    let build = queue.wait(RunClass::Build, "head build").unwrap();
    let mut small = queue
        .wait_with(RunClass::Prepare, RunBudget::new(1, 1), "aged prepare")
        .unwrap();
    small.since = Instant::now()
        .checked_sub(RunClass::Prepare.patience())
        .unwrap();
    assert!(
        admit_with_reserve(&small, 16, available, reserve).is_none(),
        "an aged waiter behind the head still keeps room for the head waiter"
    );
    assert!(
        admit_with_reserve(&build, 16, MEMORY_FLOOR + 6 * GIB - 1, reserve).is_none(),
        "even when idle, the 4 GiB floor still holds"
    );
    let running = admit_with_reserve(&build, 16, available, reserve)
        .expect("head waiter starts idle when available covers budget + floor");
    drop(build);

    let second = queue.wait(RunClass::Build, "second build").unwrap();
    assert!(
        admit_with_reserve(&second, 16, available, reserve).is_none(),
        "a second concurrent turn still obeys the full reserve"
    );
    drop(running);
    assert!(admit_with_reserve(&second, 16, available, reserve).is_some());
}

// Risk: an idle machine stuck past STARVING with no turn running must surface
// the effective idle admission condition and omit `reserve-gib` advice that
// cannot unblock an idle head admitted against the 4 GiB floor
// (mESmaC/rosen#431, mESmaC/rosen#453).
#[test]
fn idle_stall_reports_effective_condition_without_misleading_reserve_hint() {
    let dir = tempfile::tempdir().unwrap();
    let queue = RunQueue::at(dir.path());
    let headroom = MemoryHeadroom {
        available: Some(14 * GIB + GIB / 2),
        reserve: Some(12 * GIB),
    };
    let head = queue.wait(RunClass::Build, "head").unwrap();
    let now = now_nanos();
    assert!(queue.idle_stall_on(16, now, headroom).unwrap().is_none());

    let stalled = queue
        .idle_stall_on(16, now + STARVING.as_nanos(), headroom)
        .unwrap()
        .expect("head waiter past STARVING with nothing running reports a stall");
    assert_eq!(stalled.class, RunClass::Build);
    assert_eq!(stalled.suggested_reserve_gib, None);
    let summary = stalled.summary();
    assert!(!summary.contains("unblock with"), "{summary}");
    assert!(
        summary.contains("covers 10.0 GiB idle threshold"),
        "{summary}"
    );

    let too_low = MemoryHeadroom {
        available: Some(9 * GIB),
        reserve: Some(12 * GIB),
    };
    let low_stall = queue
        .idle_stall_on(16, now + STARVING.as_nanos(), too_low)
        .unwrap()
        .expect("below-floor idle stall still reports");
    assert_eq!(low_stall.suggested_reserve_gib, None);
    let low_summary = low_stall.summary();
    assert!(!low_summary.contains("unblock with"), "{low_summary}");
    assert!(
        low_summary.contains("needs 10.0 GiB with 4.0 GiB idle floor, 9.0 GiB available"),
        "{low_summary}"
    );
    let unreadable = queue
        .idle_stall_on(
            16,
            now + STARVING.as_nanos(),
            MemoryHeadroom {
                available: None,
                reserve: Some(12 * GIB),
            },
        )
        .unwrap()
        .expect("missing memory reading still reports an idle stall");
    assert_eq!(unreadable.suggested_reserve_gib, None);
    assert!(unreadable.summary().contains("no reading available"));
    assert_eq!(headroom.suggested_reserve_gib(6 * GIB), Some(8));
    assert_eq!(too_low.suggested_reserve_gib(6 * GIB), None);

    let _turn = admit_with_reserve(&head, 16, 14 * GIB, 12 * GIB).unwrap();
    assert!(queue
        .idle_stall_on(16, now + STARVING.as_nanos(), headroom)
        .unwrap()
        .is_none());
}
