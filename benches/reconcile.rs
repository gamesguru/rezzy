use std::collections::HashSet;
use std::hint::black_box;
use std::time::{Duration, Instant};

use rezzy::{
    BucketRequest, EventHash, ResidentKernel, SyndromeSketch, decode_bucket_sketches,
    estimate_delta, gf64_mul,
};

fn hash(index: u64) -> EventHash {
    let h64 = index.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17) | 1;
    EventHash {
        h128: u128::from(h64) << 64 | u128::from(index),
        h64,
    }
}

struct Xorshift128 {
    state: [u64; 2],
}

impl Xorshift128 {
    fn new(seed: u64) -> Self {
        Self {
            state: [seed, seed ^ 0x9e37_79b9_7f4a_7c15],
        }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.state[0];
        let other = self.state[1];
        value ^= value << 23;
        value ^= value >> 17;
        value ^= other ^ (other >> 26);
        self.state = [other, value];
        value
    }

    fn hash(&mut self) -> EventHash {
        let high = self.next();
        let low = self.next();
        let h64 = self.next() | 1;
        EventHash {
            h128: u128::from(high) << 64 | u128::from(low),
            h64,
        }
    }
}

fn measure(iterations: u32, mut operation: impl FnMut()) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        operation();
    }
    start.elapsed()
}

fn report(name: &str, iterations: u32, elapsed: Duration) {
    let millis = elapsed.as_secs_f64() * 1e3 / f64::from(iterations);
    println!("{name}: {millis:.6} ms/op ({iterations} iterations)");
}

fn benchmark_scale_workload() {
    const BASE_COUNT: usize = 50_000;
    const LOCAL_EXTRA_COUNT: usize = 2_100;
    const REMOTE_EXTRA_COUNT: usize = 1_900;
    let mut generator = Xorshift128::new(0x243f_6a88_85a3_08d3);
    let base: Vec<_> = (0..BASE_COUNT).map(|_| generator.hash()).collect();
    let local_extra: Vec<_> = (0..LOCAL_EXTRA_COUNT).map(|_| generator.hash()).collect();
    let remote_extra: Vec<_> = (0..REMOTE_EXTRA_COUNT).map(|_| generator.hash()).collect();
    let mut identities =
        HashSet::with_capacity(BASE_COUNT + LOCAL_EXTRA_COUNT + REMOTE_EXTRA_COUNT);
    for event in base.iter().chain(&local_extra).chain(&remote_extra) {
        assert!(identities.insert((event.h128, event.h64)));
    }
    let mut local = ResidentKernel::new();
    let mut remote = ResidentKernel::new();
    for event in &base {
        local.insert(*event).expect("benchmark hashes are valid");
        remote.insert(*event).expect("benchmark hashes are valid");
    }
    for event in &local_extra {
        local.insert(*event).expect("benchmark hashes are valid");
    }
    for event in &remote_extra {
        remote.insert(*event).expect("benchmark hashes are valid");
    }
    assert_eq!(local.accumulator().known_event_count(), 52_100);
    assert_eq!(remote.accumulator().known_event_count(), 51_900);
    assert_eq!(
        local
            .accumulator()
            .known_event_count()
            .checked_sub(remote.accumulator().known_event_count())
            .expect("local fixture is larger"),
        200
    );
    let expected_residual = local_extra
        .iter()
        .chain(&remote_extra)
        .fold(0, |residual, event| residual ^ event.h128);
    assert_eq!(
        local.accumulator().residual(remote.accumulator()),
        expected_residual
    );
    assert_eq!(LOCAL_EXTRA_COUNT + REMOTE_EXTRA_COUNT, 4_000);
    black_box((local, remote));
}

fn main() {
    let elapsed = measure(1_000_000, || {
        black_box(gf64_mul(
            black_box(0x0123_4567_89ab_cdef),
            black_box(0xfedc_ba98_7654_3211),
        ));
    });
    report("gf64 multiply", 1_000_000, elapsed);

    for count in [100, 1_000, 10_000] {
        let elapsed = measure(10, || {
            let mut resident = ResidentKernel::new();
            for index in 1..=count {
                resident
                    .insert(hash(index))
                    .expect("benchmark hashes are valid");
            }
            black_box(resident);
        });
        report(&format!("resident insert/{count}"), 10, elapsed);
    }

    for capacity in [1, 4, 8, 16, 32, 64] {
        let mut sketch = SyndromeSketch::new(capacity).expect("benchmark capacity is valid");
        for index in 1..=capacity {
            let value = u64::try_from(index)
                .expect("benchmark capacity fits u64")
                .checked_mul(2)
                .and_then(|value| value.checked_add(1))
                .expect("benchmark identifier arithmetic fits");
            sketch
                .toggle(value)
                .expect("benchmark identifiers are nonzero");
        }
        let iterations = if capacity <= 8 { 100 } else { 10 };
        let elapsed = measure(iterations, || {
            let _ = black_box(sketch.decode_elements(sketch.capacity()));
        });
        report(&format!("pinsketch decode/{capacity}"), iterations, elapsed);
    }

    let local = ResidentKernel::new();
    for count in [0, 4, 100] {
        let mut remote = ResidentKernel::new();
        for index in 1..=count {
            remote
                .insert(hash(index))
                .expect("benchmark hashes are valid");
        }
        let iterations = if count == 100 { 10 } else { 100 };
        let elapsed = measure(iterations, || {
            let _ = black_box(estimate_delta(local.strata(), remote.strata()));
        });
        report(
            &format!("triage/estimate strata/{count}"),
            iterations,
            elapsed,
        );
    }

    let requests = [BucketRequest {
        bucket_id: 0,
        capacity: 8,
    }];
    let encoded = [0_u8; 64];
    let elapsed = measure(1_000, || {
        let _ = black_box(decode_bucket_sketches(&encoded, &requests));
    });
    report("triage/parse bucket sketch", 1_000, elapsed);

    let elapsed = measure(1, benchmark_scale_workload);
    report("scale/50000 +2100/-1900", 1, elapsed);
}
