//! GF(2^256) arithmetic, syndrome construction, and decoder benchmarks.
//!
//! Run with: `cargo bench --bench rezzy -- gf256`

use std::{
    collections::HashSet,
    hint::black_box,
    time::{Duration, Instant},
};

use rezzy_recon::{
    decode_gf256_bucket_sketches_with_summaries, gf64_mul, recommended_gf256_bucket_decode_budget,
    subtract_gf256_payload, AlgebraicError, BucketRequest, FixedWindowElementHash,
    FixedWindowPopulation, Gf256, Gf256Action, Gf256BucketExchange, Gf256ClientAction,
    Gf256Decoder, Gf256FallbackReason, Gf256Sketch, IncrementalFixedWindowPopulation, NodeSummary,
    ReconciliationClient, RemoteDigest, ResidentKernel,
};
use sha2::{Digest, Sha256};

use crate::common::{measure, Xorshift128};

fn report(name: &str, iterations: u32, elapsed: std::time::Duration) {
    let nanos = elapsed.as_nanos() / u128::from(iterations);
    println!("{name}: {nanos} ns/op ({iterations} iterations)");
}

fn roots(count: usize, random: bool) -> Vec<Gf256> {
    if random {
        let mut rng = Xorshift128::new(0x6a09_e667_f3bc_c909);
        (0..count)
            .map(|_| {
                let mut limbs = [
                    rng.next_u64(),
                    rng.next_u64(),
                    rng.next_u64(),
                    rng.next_u64(),
                ];
                if limbs == [0; 4] {
                    limbs[0] = 1;
                }
                Gf256::from_limbs(limbs)
            })
            .collect()
    } else {
        (1..=count)
            .map(|index| {
                Gf256::from_limbs([
                    u64::try_from(index).expect("benchmark root fits u64"),
                    0,
                    0,
                    0,
                ])
            })
            .collect()
    }
}

fn odd_syndromes(elements: &[Gf256]) -> Vec<Gf256> {
    let mut syndromes = vec![Gf256::ZERO; elements.len()];
    for element in elements {
        let square = element.square();
        let mut power = *element;
        for syndrome in &mut syndromes {
            *syndrome = *syndrome + power;
            power = power * square;
        }
    }
    syndromes
}

fn bench_arithmetic() {
    let left = Gf256::from_limbs([
        0x0123_4567_89ab_cdef,
        0xfedc_ba98_7654_3210,
        0xdead_beef_cafe_babe,
        0x9e37_79b9_7f4a_7c15,
    ]);
    let right = Gf256::from_limbs([
        0x6a09_e667_f3bc_c909,
        0xbb67_ae85_84ca_a73b,
        0x3c6e_f372_fe94_f82b,
        0xa54f_f53a_5f1d_36f1,
    ]);
    let iterations = 200_000;
    let elapsed = measure(iterations, || {
        let _ = black_box(black_box(left) * black_box(right));
    });
    report("GF(2^256) multiply", iterations, elapsed);

    let elapsed = measure(iterations, || {
        let _ = black_box(black_box(left).square());
    });
    report("GF(2^256) square", iterations, elapsed);

    let elapsed = measure(iterations, || {
        black_box(gf64_mul(
            black_box(0x0123_4567_89ab_cdef),
            black_box(0xfedc_ba98_7654_3211),
        ));
    });
    report("GF(2^64) multiply baseline", iterations, elapsed);
}

fn bench_full_id_sketch_construction() {
    for (count, iterations) in [(8_usize, 10_000_u32), (32, 1_000), (256, 100)] {
        let elements = roots(count, true);
        let elapsed = measure(iterations, || {
            black_box(odd_syndromes(black_box(&elements)));
        });
        report(
            &format!("GF(2^256) full-ID set sketch construction/{count}"),
            iterations,
            elapsed,
        );
    }
}

fn bench_decode(decoder: &Gf256Decoder, count: usize, random: bool, iterations: u32) {
    let elements = roots(count, random);
    let syndromes = odd_syndromes(&elements);
    let label = if random { "random" } else { "consecutive" };
    let started = Instant::now();
    let decoded = decoder
        .decode(&syndromes, count)
        .expect("benchmark syndrome set decodes");
    assert_eq!(decoded.len(), count);
    println!(
        "GF(2^256) decode/{label}/{count}: warmup {:?}",
        started.elapsed()
    );
    let elapsed = measure(iterations, || {
        black_box(
            decoder
                .decode(black_box(&syndromes), count)
                .expect("benchmark syndrome set decodes"),
        );
    });
    report(
        &format!("GF(2^256) decode/{label}/{count}"),
        iterations,
        elapsed,
    );
}

#[derive(Debug)]
enum RoomPopulation {
    Cold(FixedWindowPopulation),
    Incremental(IncrementalFixedWindowPopulation),
}

impl RoomPopulation {
    fn is_incremental(&self) -> bool {
        matches!(self, Self::Incremental(_))
    }

    fn bucket_nodes(
        &self,
        requests: &[BucketRequest],
    ) -> Result<Vec<(Gf256Sketch, NodeSummary)>, AlgebraicError> {
        match self {
            Self::Cold(population) => population.bucket_nodes(requests),
            Self::Incremental(population) => population.bucket_nodes(requests),
        }
    }
}

struct RoomFixture {
    local_kernel: ResidentKernel,
    local_population: RoomPopulation,
    remote_population: RoomPopulation,
    local_digests: HashSet<[u8; 32]>,
    remote_digest: RemoteDigest,
    difference_each_way: usize,
    bucket_cache_build: Duration,
}

fn room_digest(namespace: u8, index: usize) -> [u8; 32] {
    let mut input = [0_u8; 9];
    input[0] = namespace;
    input[1..].copy_from_slice(
        &u64::try_from(index)
            .expect("room event index fits u64")
            .to_be_bytes(),
    );
    Sha256::digest(input).into()
}

fn build_room_fixture(room_events_per_side: usize, difference_each_way: usize) -> RoomFixture {
    assert!(difference_each_way <= room_events_per_side);
    let common_count = room_events_per_side - difference_each_way;
    let mut local_kernel = ResidentKernel::new();
    let mut remote_kernel = ResidentKernel::new();
    let mut local_elements = Vec::with_capacity(room_events_per_side);
    let mut remote_elements = Vec::with_capacity(room_events_per_side);
    let mut local_digests = HashSet::with_capacity(room_events_per_side);

    for index in 0..common_count {
        let digest = room_digest(0, index);
        let element = FixedWindowElementHash::from_digest32(digest);
        local_kernel
            .insert(element)
            .expect("fixture digest is valid");
        remote_kernel
            .insert(element)
            .expect("fixture digest is valid");
        local_elements.push(element);
        remote_elements.push(element);
        local_digests.insert(digest);
    }
    for index in 0..difference_each_way {
        let local_digest = room_digest(1, index);
        let local_element = FixedWindowElementHash::from_digest32(local_digest);
        local_kernel
            .insert(local_element)
            .expect("fixture digest is valid");
        local_elements.push(local_element);
        local_digests.insert(local_digest);

        let remote_element = FixedWindowElementHash::from_digest32(room_digest(2, index));
        remote_kernel
            .insert(remote_element)
            .expect("fixture digest is valid");
        remote_elements.push(remote_element);
    }

    let remote_digest = crate::common::build_remote_digest(&remote_kernel);
    let (local_population, remote_population, bucket_cache_build) =
        if std::env::var_os("REZZY_BENCH_GF256_CACHED").is_some() {
            let cache_start = Instant::now();
            let local_population =
                IncrementalFixedWindowPopulation::with_default_depth(local_elements)
                    .expect("local incremental fixture is valid");
            let remote_population =
                IncrementalFixedWindowPopulation::with_default_depth(remote_elements)
                    .expect("remote incremental fixture is valid");
            (
                RoomPopulation::Incremental(local_population),
                RoomPopulation::Incremental(remote_population),
                cache_start.elapsed(),
            )
        } else {
            (
                RoomPopulation::Cold(
                    FixedWindowPopulation::new(local_elements)
                        .expect("local fixture population is valid"),
                ),
                RoomPopulation::Cold(
                    FixedWindowPopulation::new(remote_elements)
                        .expect("remote fixture population is valid"),
                ),
                Duration::ZERO,
            )
        };
    RoomFixture {
        local_kernel,
        local_population,
        remote_population,
        local_digests,
        remote_digest,
        difference_each_way,
        bucket_cache_build,
    }
}

#[derive(Debug, Clone, Copy)]
enum RoomExchangeOutcome {
    Complete,
    Fallback(Gf256FallbackReason),
}

#[derive(Debug, Clone, Copy, Default)]
struct RoomPhaseTimes {
    plan: std::time::Duration,
    local_build: std::time::Duration,
    remote_build: std::time::Duration,
    payload: std::time::Duration,
    decode: std::time::Duration,
    verify: std::time::Duration,
}

#[derive(Debug)]
struct RoomRoundTrace {
    requests: usize,
    capacity: usize,
    successful: usize,
    failed: usize,
    next_requests: usize,
}

#[derive(Debug)]
struct RoomExchangeResult {
    outcome: RoomExchangeOutcome,
    rounds: usize,
    initial_requests: usize,
    total_requests: usize,
    sketch_payload_bytes: usize,
    phases: RoomPhaseTimes,
    round_trace: Vec<RoomRoundTrace>,
}

fn reconcile_room_once(
    fixture: &RoomFixture,
    decoder: &Gf256Decoder,
    trace_rounds: bool,
) -> Option<RoomExchangeResult> {
    let mut phases = RoomPhaseTimes::default();
    let phase_start = std::time::Instant::now();
    let client = ReconciliationClient::default().allow_unlimited_delta();
    let Gf256ClientAction::BucketSketches { requests } =
        client.select_gf256_action(&fixture.local_kernel, fixture.remote_digest, 0)
    else {
        panic!("different benchmark rooms must select bucket reconciliation");
    };
    let initial_requests = requests.len();
    let mut exchange = Gf256BucketExchange::default();
    let mut action = exchange
        .start_with_requests(requests)
        .expect("client requests cover the digest key space");
    phases.plan = phase_start.elapsed();
    let mut rounds = 0_usize;
    let mut total_requests = 0_usize;
    let mut sketch_payload_bytes = 0_usize;
    let mut round_trace = Vec::new();

    loop {
        match action {
            Gf256Action::Request(requests) => {
                rounds = rounds.saturating_add(1);
                total_requests = total_requests.saturating_add(requests.len());
                let round_capacity = requests.iter().fold(0_usize, |total, request| {
                    total.saturating_add(request.capacity)
                });
                sketch_payload_bytes =
                    sketch_payload_bytes.saturating_add(round_capacity.saturating_mul(32));
                let phase_start = std::time::Instant::now();
                let local_nodes = fixture
                    .local_population
                    .bucket_nodes(&requests)
                    .expect("local bucket construction succeeds");
                phases.local_build = phases.local_build.saturating_add(phase_start.elapsed());

                let phase_start = std::time::Instant::now();
                let remote_nodes = fixture
                    .remote_population
                    .bucket_nodes(&requests)
                    .expect("remote bucket construction succeeds");
                phases.remote_build = phases.remote_build.saturating_add(phase_start.elapsed());

                let phase_start = std::time::Instant::now();
                let (local_sketches, local_summaries): (Vec<_>, Vec<_>) =
                    local_nodes.into_iter().unzip();
                let (remote_sketches, remote_summaries): (Vec<_>, Vec<_>) =
                    remote_nodes.into_iter().unzip();
                let mut remote_payload = Vec::with_capacity(remote_sketches.len() * 32 * 32);
                for sketch in &remote_sketches {
                    remote_payload.extend_from_slice(&sketch.to_bytes());
                }
                let difference_payload =
                    subtract_gf256_payload(&local_sketches, &remote_payload, &requests)
                        .expect("same-round sketches subtract");
                phases.payload = phases.payload.saturating_add(phase_start.elapsed());

                let phase_start = std::time::Instant::now();
                let Ok(budget) = recommended_gf256_bucket_decode_budget(&requests) else {
                    // The theoretical batch allowance does not fit this
                    // target's usize (possible on 32-bit). Skip the case
                    // instead of aborting the whole benchmark.
                    return None;
                };
                let batch = decode_gf256_bucket_sketches_with_summaries(
                    &difference_payload,
                    &requests,
                    &local_summaries,
                    &remote_summaries,
                    budget,
                    decoder,
                )
                .expect("benchmark batch is well-formed");
                phases.decode = phases.decode.saturating_add(phase_start.elapsed());

                let successful = batch.successful_buckets.len();
                let failed = batch.failed_buckets.len();
                let phase_start = std::time::Instant::now();
                let next_action = exchange
                    .advance_verified(
                        batch,
                        &requests,
                        &local_summaries,
                        &remote_summaries,
                        |digest| fixture.local_digests.contains(digest),
                    )
                    .expect("verified round advances");
                phases.verify = phases.verify.saturating_add(phase_start.elapsed());
                let next_requests = match &next_action {
                    Gf256Action::Request(requests) => requests.len(),
                    Gf256Action::Complete { .. } | Gf256Action::Fallback { .. } => 0,
                };
                if trace_rounds {
                    round_trace.push(RoomRoundTrace {
                        requests: requests.len(),
                        capacity: round_capacity,
                        successful,
                        failed,
                        next_requests,
                    });
                }
                action = next_action;
            }
            Gf256Action::Complete {
                local_only,
                remote_only,
            } => {
                assert_eq!(local_only.len(), fixture.difference_each_way);
                assert_eq!(remote_only.len(), fixture.difference_each_way);
                return Some(RoomExchangeResult {
                    outcome: RoomExchangeOutcome::Complete,
                    rounds,
                    initial_requests,
                    total_requests,
                    sketch_payload_bytes,
                    phases,
                    round_trace,
                });
            }
            Gf256Action::Fallback { reason } => {
                return Some(RoomExchangeResult {
                    outcome: RoomExchangeOutcome::Fallback(reason),
                    rounds,
                    initial_requests,
                    total_requests,
                    sketch_payload_bytes,
                    phases,
                    round_trace,
                });
            }
        }
    }
}

fn bench_room_reconciliation(room_events: usize, difference_each_way: usize) {
    // Population construction, digest generation, and decoder construction
    // are deliberately outside the timed region. Each sample is one complete
    // client/server exchange over prebuilt room snapshots.
    let fixture = build_room_fixture(room_events, difference_each_way);
    let decoder = Gf256Decoder::build().expect("GF(2^256) decoder builds");
    let trace_rounds = std::env::var_os("REZZY_BENCH_GF256_ROUND_TRACE").is_some();
    let mut outcome = None;
    let elapsed = measure(1, || {
        outcome = Some(reconcile_room_once(
            black_box(&fixture),
            &decoder,
            trace_rounds,
        ));
    });
    let Some(result) = outcome.flatten() else {
        println!(
            "skipping {room_events}-event room with {difference_each_way} differences: \
             round work budget does not fit usize"
        );
        return;
    };
    let peer_raw_bytes = room_events.saturating_mul(32);
    let sketch_pct = (result.sketch_payload_bytes as f64 / peer_raw_bytes as f64) * 100.0;
    let outcome_str = match result.outcome {
        RoomExchangeOutcome::Complete => "Complete".to_string(),
        RoomExchangeOutcome::Fallback(reason) => {
            let mut reason = reason.to_string();
            if let Some(first) = reason.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            reason
        }
    };
    let room_label = if room_events >= 1_000_000 {
        format!("{}M", room_events / 1_000_000)
    } else {
        format!("{}k", room_events / 1_000)
    };
    let bucket_build = result.phases.local_build + result.phases.remote_build;
    let cache_ms = if fixture.local_population.is_incremental() {
        format!("{:.1}", fixture.bucket_cache_build.as_secs_f64() * 1e3)
    } else {
        "-".to_string()
    };
    println!(
        "{:<4} {:>5} {:>7} {:>9.1} {:>5.1} {:>6.1} {:>7.1} {:>6.1} {:>6.1} {:<11} {:>6} {:>5}/{:<5} {:>8} {:>8.3}",
        room_label,
        difference_each_way,
        cache_ms,
        elapsed.as_secs_f64() * 1e3,
        result.phases.plan.as_secs_f64() * 1e3,
        bucket_build.as_secs_f64() * 1e3,
        result.phases.payload.as_secs_f64() * 1e3,
        result.phases.decode.as_secs_f64() * 1e3,
        result.phases.verify.as_secs_f64() * 1e3,
        outcome_str,
        result.rounds,
        result.initial_requests,
        result.total_requests,
        result.sketch_payload_bytes,
        sketch_pct,
    );

    if trace_rounds
        && (result.rounds > 1 || matches!(result.outcome, RoomExchangeOutcome::Fallback(_)))
    {
        for (index, round) in result.round_trace.iter().enumerate() {
            println!(
                "  round {:>2}: {:>3} requests, capacity {:>4}, {:>3} succeeded, {:>3} failed, {:>3} next-round requests",
                index + 1,
                round.requests,
                round.capacity,
                round.successful,
                round.failed,
                round.next_requests,
            );
        }
    }
}

fn bench_room_matrix() {
    println!(
        "\nGF256 room (in-process; no setup/net/framing; times in ms; sketch_B excludes summaries/framing)"
    );
    println!("REZZY_BENCH_GF256_ROUND_TRACE=1: per-round counts for multi-round/fallback cases.");
    println!("REZZY_BENCH_GF256_CACHED=1: Bucket uses the incremental cache; Cache is fixture build time outside Total.");
    println!();
    println!(
        "{:<4} {:>5} {:>7} {:>9} {:>5} {:>6} {:>7} {:>6} {:>6} {:<11} {:>6} {:<11} {:>8} {:>8}",
        "Pop",
        "Diff",
        "Cache",
        "Total ms",
        "Plan",
        "Bucket",
        "Payload",
        "Decode",
        "Verify",
        "Result",
        "Rounds",
        "Init/Total",
        "Sketch B",
        "% Peer"
    );
    for (room_events, differences) in [
        (100_000_usize, 10_usize),
        (100_000, 100),
        (100_000, 1_000),
        (100_000, 10_000),
        (1_000_000, 10),
        (1_000_000, 100),
        (1_000_000, 1_000),
    ] {
        bench_room_reconciliation(room_events, differences);
    }

    // The largest case is available on demand because its population setup
    // and full exchange can be substantial on developer machines.
    if std::env::var_os("REZZY_BENCH_GF256_1M_10K").is_some() {
        bench_room_reconciliation(1_000_000, 10_000);
    }
}

pub fn run() {
    println!("GF(2^256) profile benchmarks (cached quadratic solver)");
    bench_arithmetic();
    bench_full_id_sketch_construction();

    let decoder = Gf256Decoder::build().expect("GF(2^256) quadratic solver builds");
    bench_decode(&decoder, 8, false, 20);
    bench_decode(&decoder, 32, false, 5);
    bench_decode(&decoder, 256, false, 1);
    bench_decode(&decoder, 256, true, 1);
    bench_room_matrix();
}
