//! Phase timings for R8 landing: always accumulated per request for `stats`,
//! with stderr logging kept opt-in behind `ROSEN_LAND_TIMINGS` (#449, #471).

use std::cell::RefCell;
use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rosen_core::ChangeId;
use rosen_protocol::LandPhaseTimings;

static STDERR_ENABLED: OnceLock<bool> = OnceLock::new();

struct LogEntry {
    change: String,
    scope: &'static str,
    phase: String,
    elapsed: Duration,
}

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
    capture_log: bool,
    entries: Vec<LogEntry>,
}

impl Accumulator {
    fn new(capture_log: bool) -> Self {
        Self {
            capture_log,
            ..Self::default()
        }
    }

    fn add(&mut self, scope: &'static str, phase: &str, elapsed: Duration, change: Option<&str>) {
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
        if self.capture_log {
            self.entries.push(LogEntry {
                change: change.unwrap_or_default().to_owned(),
                scope,
                phase: phase.to_owned(),
                elapsed,
            });
        }
    }

    fn into_parts(self, reported_attempts: Option<u32>) -> (LandPhaseTimings, Vec<LogEntry>) {
        let timings = LandPhaseTimings {
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
        };
        (timings, self.entries)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Accumulator>> = const { RefCell::new(None) };
}

/// Captures every [`Timing`] phase on the current thread for one land request.
/// When opt-in stderr logging is active, entries are buffered until the request
/// finishes so an atomic stack land attributes every phase to all landed layers.
pub(super) struct Collector {
    previous: Option<Accumulator>,
    finished: bool,
}

impl Collector {
    pub(super) fn start() -> Self {
        Self::start_with_log(stderr_enabled())
    }

    fn start_with_log(capture_log: bool) -> Self {
        let previous =
            CURRENT.with(|slot| slot.borrow_mut().replace(Accumulator::new(capture_log)));
        Self {
            previous,
            finished: false,
        }
    }

    pub(super) fn finish(mut self, attempts: Option<u32>) -> LandPhaseTimings {
        let (timings, entries) = self.take_current().into_parts(attempts);
        flush_entries(&entries, None, &[]);
        timings
    }

    pub(super) fn finish_for_layers(
        mut self,
        attempts: Option<u32>,
        top: &ChangeId,
        ancestors: &[ChangeId],
    ) -> LandPhaseTimings {
        let (timings, entries) = self.take_current().into_parts(attempts);
        flush_entries(&entries, Some(top), ancestors);
        timings
    }

    fn take_current(&mut self) -> Accumulator {
        self.finished = true;
        CURRENT.with(|slot| {
            let taken = slot.borrow_mut().take().unwrap_or_default();
            *slot.borrow_mut() = self.previous.take();
            taken
        })
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        if !self.finished {
            let current = self.take_current();
            flush_entries(&current.entries, None, &[]);
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
        Self {
            change: logging_active().then(|| change.to_string()),
            scope,
            started,
            previous: started,
        }
    }

    pub(super) fn phase(&mut self, phase: &str) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.previous);
        self.record_or_log(phase, elapsed);
        self.previous = now;
    }

    fn record_or_log(&self, phase: &str, elapsed: Duration) {
        if record(self.scope, phase, elapsed, self.change.as_deref()) {
            return;
        }
        if let Some(change) = &self.change {
            eprintln!("{}", format_line(change, self.scope, phase, elapsed, None));
        }
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        self.record_or_log("total", elapsed);
    }
}

fn stderr_enabled() -> bool {
    *STDERR_ENABLED.get_or_init(|| std::env::var_os("ROSEN_LAND_TIMINGS").is_some())
}

fn logging_active() -> bool {
    stderr_enabled()
        || CURRENT.with(|slot| slot.borrow().as_ref().is_some_and(|acc| acc.capture_log))
}

fn record(scope: &'static str, phase: &str, elapsed: Duration, change: Option<&str>) -> bool {
    CURRENT.with(|slot| match slot.borrow_mut().as_mut() {
        Some(acc) => {
            acc.add(scope, phase, elapsed, change);
            true
        }
        None => false,
    })
}

fn flush_entries(entries: &[LogEntry], top: Option<&ChangeId>, ancestors: &[ChangeId]) {
    if entries.is_empty() {
        return;
    }
    let lines = format_entries(entries, top, ancestors);
    let mut err = std::io::stderr().lock();
    for line in lines {
        let _ = writeln!(err, "{line}");
    }
}

fn format_entries(
    entries: &[LogEntry],
    top: Option<&ChangeId>,
    ancestors: &[ChangeId],
) -> Vec<String> {
    let Some(top) = top else {
        return entries
            .iter()
            .map(|e| format_line(&e.change, e.scope, &e.phase, e.elapsed, None))
            .collect();
    };
    let top_id = top.to_string();
    let mut lines = Vec::with_capacity(entries.len() * (ancestors.len() + 1));
    for e in entries {
        lines.push(format_line(&top_id, e.scope, &e.phase, e.elapsed, None));
    }
    for ancestor in ancestors {
        if ancestor == top {
            continue;
        }
        let layer_id = ancestor.to_string();
        for e in entries {
            lines.push(format_line(
                &layer_id,
                e.scope,
                &e.phase,
                e.elapsed,
                Some(&top_id),
            ));
        }
    }
    lines
}

fn format_line(
    change: &str,
    scope: &str,
    phase: &str,
    elapsed: Duration,
    shared_with: Option<&str>,
) -> String {
    let ms = elapsed.as_secs_f64() * 1000.0;
    match shared_with {
        Some(top) => format!(
            "land_timing change={change} scope={scope} phase={phase} ms={ms:.3} shared_with={top}"
        ),
        None => format!("land_timing change={change} scope={scope} phase={phase} ms={ms:.3}"),
    }
}

fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Risk (#471, R8): opt-in `ROSEN_LAND_TIMINGS` logs must attribute every
    /// phase of an atomic stack land to all landed layers while marking shared
    /// lower layers with the top change so readers do not double-count samples.
    #[test]
    fn atomic_stack_log_attributes_all_layers_with_shared_top() {
        let lower = ChangeId::from_bytes([1; 16]);
        let upper = ChangeId::from_bytes([2; 16]);
        let mut collector = Collector::start_with_log(true);
        {
            let mut req = Timing::new(upper, "request");
            req.phase("registry");
            {
                let mut commit = Timing::new(upper, "commit");
                commit.phase("write");
            }
        }
        let (_, entries) = collector.take_current().into_parts(Some(1));
        let lines = format_entries(&entries, Some(&upper), &[lower, upper]);
        let upper_lines: Vec<_> = lines
            .iter()
            .filter(|l| l.contains(&format!("change={upper} ")))
            .collect();
        let lower_lines: Vec<_> = lines
            .iter()
            .filter(|l| l.contains(&format!("change={lower} ")))
            .collect();
        assert_eq!(upper_lines.len(), entries.len());
        assert_eq!(lower_lines.len(), entries.len());
        assert!(upper_lines.iter().all(|l| !l.contains("shared_with=")));
        assert!(lower_lines
            .iter()
            .all(|l| l.ends_with(&format!(" shared_with={upper}"))));
    }
}
