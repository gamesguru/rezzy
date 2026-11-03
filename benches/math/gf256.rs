//! GF(2^256) arithmetic, syndrome construction, and decoder benchmarks.
//!
//! Run with: `cargo bench --bench rezzy -- gf256`

use std::{hint::black_box, time::Instant};

use rezzy_recon::{gf64_mul, Gf256, Gf256Decoder};

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
                let mut limbs = [rng.next_u64(), rng.next_u64(), rng.next_u64(), rng.next_u64()];
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
        black_box(gf64_mul(black_box(0x0123_4567_89ab_cdef), black_box(0xfedc_ba98_7654_3211)));
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
    report(&format!("GF(2^256) decode/{label}/{count}"), iterations, elapsed);
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
}
