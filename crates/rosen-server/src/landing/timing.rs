//! Opt-in phase timings for diagnosing R8 latency under concurrent lands.
use std::sync::OnceLock;
use std::time::Instant;

static ENABLED: OnceLock<bool> = OnceLock::new();

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
            change: (*ENABLED.get_or_init(|| std::env::var_os("ROSEN_LAND_TIMINGS").is_some()))
                .then(|| change.to_string()),
            scope,
            started,
            previous: started,
        }
    }

    pub(super) fn phase(&mut self, phase: &str) {
        let now = Instant::now();
        if let Some(change) = &self.change {
            eprintln!(
                "land_timing change={change} scope={} phase={phase} ms={:.3}",
                self.scope,
                now.duration_since(self.previous).as_secs_f64() * 1000.0
            );
        }
        self.previous = now;
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        if let Some(change) = &self.change {
            eprintln!(
                "land_timing change={change} scope={} phase=total ms={:.3}",
                self.scope,
                self.started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}
