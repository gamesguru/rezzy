// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Lane-parallel GF(2^256) multiply-accumulate for polynomial loops.
//!
//! The scalar field multiply already uses PCLMULQDQ for each 64x64
//! carry-less product. This module processes four coefficients at once:
//! each 128-bit AVX-512 lane holds one coefficient's 64-bit limb, so one
//! vector carry-less multiply computes the same schoolbook product for four
//! coefficients. Reduction remains scalar per coefficient.
//!
//! The vector backend is compiled for `std` on `x86_64` and is selected at
//! runtime when the CPU reports `avx512f` and `vpclmulqdq`. Other targets and
//! `no_std` builds use the scalar fallback.

#![allow(unsafe_code)]

use crate::binary_field::BinaryField;
use crate::gf256::Gf256;

/// `target[i] ^= term * source[i]`, with a lane-parallel backend when
/// available.
pub(crate) fn poly_mac(term: Gf256, source: &[Gf256], target: &mut [Gf256]) {
    debug_assert_eq!(source.len(), target.len());

    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    if std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("vpclmulqdq") {
        // SAFETY: the runtime checks above prove both required feature sets.
        unsafe { poly_mac_avx512(term, source, target) };
        return;
    }

    scalar_poly_mac(term, source, target);
}

fn scalar_poly_mac(term: Gf256, source: &[Gf256], target: &mut [Gf256]) {
    for (coefficient, slot) in source.iter().zip(target.iter_mut()) {
        *slot = BinaryField::add(*slot, BinaryField::mul(term, *coefficient));
    }
}

#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f,vpclmulqdq")]
#[allow(clippy::arithmetic_side_effects)]
unsafe fn poly_mac_avx512(term: Gf256, source: &[Gf256], target: &mut [Gf256]) {
    use core::arch::x86_64::{_mm512_clmulepi64_epi128, _mm512_storeu_si512};

    const LANES: usize = 4;
    let term_limbs = term.limbs();
    let term_vectors = [
        broadcast(term_limbs[0]),
        broadcast(term_limbs[1]),
        broadcast(term_limbs[2]),
        broadcast(term_limbs[3]),
    ];

    let mut start = 0_usize;
    while start + LANES <= source.len() {
        let mut coefficient_limbs = [[0_u64; LANES]; 4];
        for (offset, coefficient) in source[start..start + LANES].iter().enumerate() {
            let limbs = coefficient.limbs();
            for (lane, limb) in coefficient_limbs.iter_mut().zip(limbs) {
                lane[offset] = limb;
            }
        }
        let coefficient_vectors = [
            pack(coefficient_limbs[0]),
            pack(coefficient_limbs[1]),
            pack(coefficient_limbs[2]),
            pack(coefficient_limbs[3]),
        ];

        // Schoolbook 4x4 carry-less products. Each vector holds the same
        // (i, j) product for four coefficients in its four 128-bit lanes.
        let mut products = [[0_u64; 8]; 16];
        for (i, left) in coefficient_vectors.iter().enumerate() {
            for (j, right) in term_vectors.iter().enumerate() {
                let product = _mm512_clmulepi64_epi128(*left, *right, 0x00);
                let slot = &mut products[i * 4 + j];
                // SAFETY: `slot` is an aligned-independent scratch array large
                // enough for one 512-bit store.
                unsafe { _mm512_storeu_si512(slot.as_mut_ptr().cast(), product) };
            }
        }

        for lane in 0..LANES {
            let mut limbs = [0_u64; 8];
            for i in 0..4 {
                for j in 0..4 {
                    let product = &products[i * 4 + j];
                    limbs[i + j] ^= product[2 * lane];
                    limbs[i + j + 1] ^= product[2 * lane + 1];
                }
            }
            target[start + lane] =
                BinaryField::add(target[start + lane], crate::gf256::reduce_limbs(limbs));
        }
        start += LANES;
    }

    for offset in start..source.len() {
        target[offset] = BinaryField::add(target[offset], BinaryField::mul(term, source[offset]));
    }
}

#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
fn broadcast(value: u64) -> core::arch::x86_64::__m512i {
    pack([value; 4])
}

#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
fn pack(values: [u64; 4]) -> core::arch::x86_64::__m512i {
    use core::arch::x86_64::_mm512_set_epi64;

    let to_i64 = |value: u64| i64::from_ne_bytes(value.to_ne_bytes());
    // Place one value in the low 64 bits of each 128-bit lane and zero the
    // high half. `_mm512_clmulepi64_epi128(..., 0x00)` selects that half.
    _mm512_set_epi64(
        0,
        to_i64(values[3]),
        0,
        to_i64(values[2]),
        0,
        to_i64(values[1]),
        0,
        to_i64(values[0]),
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn element(state: &mut u64) -> Gf256 {
        Gf256::from_limbs([next(state), next(state), next(state), next(state)])
    }

    fn mac_fixture(seed: u64) -> (Gf256, alloc::vec::Vec<Gf256>, alloc::vec::Vec<Gf256>) {
        let mut state = seed;
        let term = element(&mut state);
        let source = (0..17).map(|_| element(&mut state)).collect();
        let initial = (0..17).map(|_| element(&mut state)).collect();
        (term, source, initial)
    }

    #[test]
    fn scalar_mac_matches_reference() {
        let (term, source, initial) = mac_fixture(0x243f_6a88_85a3_08d3_u64);
        let mut expected = initial.clone();
        for (coefficient, slot) in source.iter().zip(&mut expected) {
            *slot = BinaryField::add(*slot, BinaryField::mul(term, *coefficient));
        }
        let mut actual = initial;
        scalar_poly_mac(term, &source, &mut actual);
        assert_eq!(actual, expected);
    }

    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    #[test]
    fn lane_parallel_mac_matches_scalar() {
        if !std::is_x86_feature_detected!("avx512f") || !std::is_x86_feature_detected!("vpclmulqdq")
        {
            return;
        }
        let (term, source, initial) = mac_fixture(0x9e37_79b9_7f4a_7c15_u64);
        let mut expected = initial.clone();
        let mut actual = initial;
        scalar_poly_mac(term, &source, &mut expected);
        // SAFETY: the runtime checks above establish both target features.
        unsafe { poly_mac_avx512(term, &source, &mut actual) };
        assert_eq!(actual, expected);
    }
}
