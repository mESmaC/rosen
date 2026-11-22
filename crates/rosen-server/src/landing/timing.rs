//! Phase timings for R8 landing: always accumulated per request for `stats`,
//! with stderr logging kept opt-in behind `ROSEN_LAND_TIMINGS` (#449).

use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rosen_protocol::LandPhaseTimings;

static STDERR_ENABLED: OnceLock<bool> = OnceLock::new();

#[derive(Default)]
struct Accumulator {
    registry: Duration,
    engine: Duration,
    attempts: Duration,
    facts: Duration,
    tiers: Duration,
    merge: Duration,
    sizes: Duration,
    receipt: Duration,
    write: Duration,
    cas: Duration,
    write_and_cas: Duration,
    restack: Duration,
    decision_and_restack: Duration,
    answer: Duration,
    total: Duration,
    attempt_count: u32,
}

impl Accumulator {
    fn add(&mut self, scope: &str, phase: &str, elapsed: Duration) {
        match (scope, phase) {
            ("request", "registry") => self.registry += elapsed,
            ("request", "engine") => self.engine += elapsed,
            ("request", "decision_and_restack") => self.decision_and_restack += elapsed,
            ("request", "answer") => self.answer += elapsed,
            ("request", "total") => self.total += elapsed,
            ("decision", "attempts") => self.attempts += elapsed,
            ("decision", "restack") => self.restack += elapsed,
            ("attempt", "facts") => self.facts += elapsed,
            ("attempt", "tiers") => self.tiers += elapsed,
            ("attempt", "merge") => self.merge += elapsed,
            ("attempt", "sizes") => self.sizes += elapsed,
            ("attempt", "receipt") => self.receipt += elapsed,
            ("attempt", "write_and_cas") => self.write_and_cas += elapsed,
            ("attempt", "total") => self.attempt_count = self.attempt_count.saturating_add(1),
            ("commit", "write") => self.write += elapsed,
            ("commit", "cas") => self.cas += elapsed,
            _ => {}
        }
    }

    fn into_timings(self, reported_attempts: Option<u32>) -> LandPhaseTimings {
        LandPhaseTimings {
            attempts: reported_attempts.unwrap_or(self.attempt_count),
            registry_ms: ms(self.registry),
            engine_ms: ms(self.engine),
            attempts_ms: ms(self.attempts),
            facts_ms: ms(self.facts),
            tiers_ms: ms(self.tiers),
            merge_ms: ms(self.merge),
            sizes_ms: ms(self.sizes),
            receipt_ms: ms(self.receipt),
            write_ms: ms(self.write),
            cas_ms: ms(self.cas),
            write_and_cas_ms: ms(self.write_and_cas),
            restack_ms: ms(self.restack),
            decision_and_restack_ms: ms(self.decision_and_restack),
            answer_ms: ms(self.answer),
            total_ms: ms(self.total),
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Accumulator>> = const { RefCell::new(None) };
}

/// Captures every [`Timing`] phase on the current thread for one land request.
pub(super) struct Collector {
    previous: Option<Accumulator>,
    finished: bool,
}

impl Collector {
    pub(super) fn start() -> Self {
        let previous = CURRENT.with(|slot| slot.borrow_mut().replace(Accumulator::default()));
        Self {
            previous,
            finished: false,
        }
    }

    pub(super) fn finish(mut self, attempts: Option<u32>) -> LandPhaseTimings {
        self.finished = true;
        let current = CURRENT.with(|slot| {
            let taken = slot.borrow_mut().take().unwrap_or_default();
            *slot.borrow_mut() = self.previous.take();
            taken
        });
        current.into_timings(attempts)
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        if !self.finished {
            let previous = self.previous.take();
            CURRENT.with(|slot| {
                *slot.borrow_mut() = previous;
            });
        }
    }
}

pub(super) struct Timing {
    change: Option<String>,
    scope: &'static str,
    started: Instant,
    previous: Instant,
}

impl Timing {
    pub(super) fn new(change: impl std::fmt::Display, scope: &'static str) -> Self {
        let started = Instant::now();
        let stderr =
            *STDERR_ENABLED.get_or_init(|| std::env::var_os("ROSEN_LAND_TIMINGS").is_some());
        Self {
            change: stderr.then(|| change.to_string()),
            scope,
            started,
            previous: started,
        }
    }

    pub(super) fn phase(&mut self, phase: &str) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.previous);
        record(self.scope, phase, elapsed);
        if let Some(change) = &self.change {
            eprintln!(
                "land_timing change={change} scope={} phase={phase} ms={:.3}",
                self.scope,
                elapsed.as_secs_f64() * 1000.0
            );
        }
        self.previous = now;
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        record(self.scope, "total", elapsed);
        if let Some(change) = &self.change {
            eprintln!(
                "land_timing change={change} scope={} phase=total ms={:.3}",
                self.scope,
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }
}

fn record(scope: &str, phase: &str, elapsed: Duration) {
    CURRENT.with(|slot| {
        if let Some(acc) = slot.borrow_mut().as_mut() {
            acc.add(scope, phase, elapsed);
        }
    });
}

fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
