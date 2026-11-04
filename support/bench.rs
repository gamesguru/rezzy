//! Shared timing and deterministic-PRNG helpers for benchmark harnesses.

use std::time::{Duration, Instant};

/// Runs `operation` `iterations` times and returns the elapsed wall time.
pub fn measure(iterations: u32, mut operation: impl FnMut()) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        operation();
    }
    start.elapsed()
}

/// Deterministic PRNG (`xorshift128+`) so bench inputs are reproducible
/// without adding a `rand` dependency.
pub struct Xorshift128 {
    state: [u64; 2],
}

impl Xorshift128 {
    pub fn new(seed: u64) -> Self {
        Self {
            state: [seed ^ 0x9E37_79B9_7F4A_7C15, seed.wrapping_add(1) | 1],
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state[0];
        let y = self.state[1];
        self.state[0] = y;
        x ^= x << 23;
        x ^= x >> 17;
        x ^= y ^ (y >> 26);
        self.state[1] = x;
        x.wrapping_add(y)
    }
}
