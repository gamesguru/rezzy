//! GF(2^256) arithmetic, syndrome construction, and decoder benchmarks.
//!
//! Run with: `cargo bench --bench rezzy -- gf256`

use std::{collections::HashSet, hint::black_box, time::Instant};

use rezzy_recon::{
    decode_gf256_bucket_sketches_with_summaries, gf64_mul, recommended_bucket_decode_budget,
    subtract_gf256_payload, FixedWindowElementHash, FixedWindowPopulation, Gf256, Gf256Action,
    Gf256BucketExchange, Gf256ClientAction, Gf256Decoder, ReconciliationClient, RemoteDigest,
    ResidentKernel,
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

struct RoomFixture {
    local_kernel: ResidentKernel,
    local_population: FixedWindowPopulation,
    remote_population: FixedWindowPopulation,
    local_digests: HashSet<[u8; 32]>,
    remote_digest: RemoteDigest,
    difference_each_way: usize,
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
    RoomFixture {
        local_kernel,
        local_population: FixedWindowPopulation::new(local_elements)
            .expect("local fixture population is valid"),
        remote_population: FixedWindowPopulation::new(remote_elements)
            .expect("remote fixture population is valid"),
        local_digests,
        remote_digest,
        difference_each_way,
    }
}

#[derive(Debug, Clone, Copy)]
enum RoomExchangeOutcome {
    Complete,
    Fallback,
}

fn reconcile_room_once(fixture: &RoomFixture, decoder: &Gf256Decoder) -> RoomExchangeOutcome {
    let client = ReconciliationClient::default().allow_unlimited_delta();
    let Gf256ClientAction::BucketSketches { requests } =
        client.select_gf256_action(&fixture.local_kernel, fixture.remote_digest, 0)
    else {
        panic!("different benchmark rooms must select bucket reconciliation");
    };
    let mut exchange = Gf256BucketExchange::default();
    let mut action = exchange
        .start_with_requests(requests)
        .expect("client requests cover the digest key space");

    loop {
        match action {
            Gf256Action::Request(requests) => {
                let local_nodes = fixture
                    .local_population
                    .bucket_nodes(&requests)
                    .expect("local bucket construction succeeds");
                let remote_nodes = fixture
                    .remote_population
                    .bucket_nodes(&requests)
                    .expect("remote bucket construction succeeds");
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
                let budget = recommended_bucket_decode_budget(&requests)
                    .expect("round work budget fits usize");
                let batch = decode_gf256_bucket_sketches_with_summaries(
                    &difference_payload,
                    &requests,
                    &local_summaries,
                    &remote_summaries,
                    budget,
                    decoder,
                )
                .expect("benchmark batch is well-formed");
                action = exchange
                    .advance_verified(
                        batch,
                        &requests,
                        &local_summaries,
                        &remote_summaries,
                        |digest| fixture.local_digests.contains(digest),
                    )
                    .expect("verified round advances");
            }
            Gf256Action::Complete {
                local_only,
                remote_only,
            } => {
                assert_eq!(local_only.len(), fixture.difference_each_way);
                assert_eq!(remote_only.len(), fixture.difference_each_way);
                return RoomExchangeOutcome::Complete;
            }
            Gf256Action::Fallback => return RoomExchangeOutcome::Fallback,
        }
    }
}

fn bench_room_reconciliation(room_events: usize, difference_each_way: usize) {
    // Population construction, digest generation, and decoder construction
    // are deliberately outside the timed region. Each sample is one complete
    // client/server exchange over prebuilt room snapshots.
    let fixture = build_room_fixture(room_events, difference_each_way);
    let decoder = Gf256Decoder::build().expect("GF(2^256) decoder builds");
    let mut outcome = None;
    let elapsed = measure(1, || {
        outcome = Some(reconcile_room_once(black_box(&fixture), &decoder));
    });
    println!(
        "GF256 room exchange/{room_events}/diff_each_way_{difference_each_way}: {:.3} ms/op (one exchange; result={:?}; setup excluded)",
        elapsed.as_secs_f64() * 1e3,
        outcome.expect("one benchmark iteration ran"),
    );
}

fn bench_room_matrix() {
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
