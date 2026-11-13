//! A tiny seeded generator, so a failing fault run replays exactly from its
//! printed seed. SplitMix64: no dependency, and statistical quality is
//! plenty for picking kill times and victims.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// A seed from the OS, for runs that do not ask for one.
    pub fn fresh_seed() -> u64 {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).expect("OS randomness");
        u64::from_le_bytes(bytes)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `low..=high`.
    pub fn between(&mut self, low: Duration, high: Duration) -> Duration {
        let span = high.saturating_sub(low).as_millis() as u64;
        low + Duration::from_millis(self.below(span + 1))
    }

    /// An exponential gap with the given mean: kills arrive as a Poisson
    /// process, so they land at arbitrary points of every operation rather
    /// than on a beat the system could fall into step with.
    pub fn exponential(&mut self, mean: Duration) -> Duration {
        let draw = -(1.0 - self.unit()).ln();
        mean.mul_f64(draw.min(20.0))
    }
}
