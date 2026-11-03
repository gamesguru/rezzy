// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Experimental GF(2^256) polynomial-basis field.
//!
//! The representation is four little-endian `u64` limbs, with bit `i`
//! representing the coefficient of `x^i`. Reduction uses the irreducible
//! polynomial `x^256 + x^10 + x^5 + x^2 + 1`, verified by Rabin's test in
//! the `binary_field` test module. The little-endian limb/byte mapping is
//! fixed by this profile. The current decoder ceiling is 300 million work
//! units, based on release measurements of degree-256 consecutive roots
//! (232.6M units) and pseudorandom roots (36.1M units). It is a practical
//! budget, not a worst-case factorization bound.

use crate::binary_field::BinaryField;
use crate::pinsketch::QuadraticSolver;
use alloc::vec::Vec;

/// Element of GF(2^256) in the fixed four-limb polynomial basis.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[must_use]
pub struct Gf256([u64; 4]);

impl Gf256 {
    /// The additive identity.
    pub const ZERO: Self = Self([0; 4]);
    /// The multiplicative identity.
    pub const ONE: Self = Self([1, 0, 0, 0]);

    /// Constructs an element from little-endian 64-bit polynomial limbs.
    pub const fn from_limbs(limbs: [u64; 4]) -> Self {
        Self(limbs)
    }

    /// Returns the little-endian 64-bit polynomial limbs.
    #[must_use]
    pub const fn limbs(self) -> [u64; 4] {
        self.0
    }

    /// Parses exactly 32 little-endian bytes.
    #[must_use]
    pub fn from_le_bytes(bytes: &[u8]) -> Option<Self> {
        <Self as BinaryField>::from_le_bytes(bytes)
    }

    /// Serializes this element as 32 little-endian bytes.
    #[must_use]
    pub fn to_le_bytes(self) -> [u8; 32] {
        <Self as BinaryField>::to_le_bytes(self)
    }

    /// Squares this element in the polynomial basis.
    pub fn square(self) -> Self {
        <Self as BinaryField>::square(self)
    }

    /// Returns the multiplicative inverse, or `None` for zero.
    #[must_use]
    pub fn inv(self) -> Option<Self> {
        <Self as BinaryField>::inv(self)
    }

    /// Decodes odd-power syndromes using the current GF(2^256) work ceiling.
    /// The ceiling is based on measured consecutive and pseudorandom degree-256
    /// inputs; adversarial factorization shapes can still exhaust it.
    /// In `no_std`, this convenience method rebuilds the quadratic solver;
    /// use [`Gf256Decoder::build`] to reuse it across calls.
    ///
    /// # Errors
    /// Returns `DecodeFailure` for invalid or inconsistent syndromes and
    /// `BudgetExhausted` when factoring exceeds the configured work ceiling.
    pub fn decode(
        odd_syndromes: &[Self],
        max_elements: usize,
    ) -> Result<alloc::vec::Vec<Self>, crate::algebraic::AlgebraicError> {
        #[cfg(feature = "std")]
        let solver = Some(cached_quadratic_solver());
        #[cfg(feature = "std")]
        return crate::pinsketch::decode_with_model(
            odd_syndromes,
            max_elements,
            crate::pinsketch::WorkModel::GF256,
            solver,
        );
        #[cfg(not(feature = "std"))]
        Gf256Decoder::build()
            .ok_or(crate::algebraic::AlgebraicError::DecodeFailure)?
            .decode(odd_syndromes, max_elements)
    }
}

/// Reusable GF(2^256) decoder. Construct one and call [`Self::decode`] on it
/// to reuse the quadratic solver, including in `no_std` environments.
pub struct Gf256Decoder {
    quadratic_solver: QuadraticSolver<Gf256>,
}

impl Gf256Decoder {
    /// Builds the quadratic solver once for reuse by future decodes.
    #[must_use]
    pub fn build() -> Option<Self> {
        Some(Self {
            quadratic_solver: QuadraticSolver::new()?,
        })
    }

    /// Decodes odd-power syndromes with this decoder's cached quadratic solver.
    ///
    /// # Errors
    /// Returns `DecodeFailure` for invalid or inconsistent syndromes and
    /// `BudgetExhausted` when factoring exceeds the configured work ceiling.
    pub fn decode(
        &self,
        odd_syndromes: &[Gf256],
        max_elements: usize,
    ) -> Result<Vec<Gf256>, crate::algebraic::AlgebraicError> {
        crate::pinsketch::decode_with_model(
            odd_syndromes,
            max_elements,
            crate::pinsketch::WorkModel::GF256,
            Some(&self.quadratic_solver),
        )
    }
}

#[cfg(feature = "std")]
pub(crate) fn cached_quadratic_solver() -> &'static QuadraticSolver<Gf256> {
    use std::sync::OnceLock;

    static SOLVER: OnceLock<QuadraticSolver<Gf256>> = OnceLock::new();
    SOLVER
        .get_or_init(|| QuadraticSolver::new().expect("GF(2^256) quadratic map has a solved form"))
}

impl core::ops::Add for Gf256 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        <Self as BinaryField>::add(self, rhs)
    }
}

impl core::ops::Mul for Gf256 {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        <Self as BinaryField>::mul(self, rhs)
    }
}

#[derive(Clone, Copy)]
struct Wide([u64; 8]);

impl Wide {
    fn xor_at(&mut self, offset: usize, value: &[u64]) -> Option<()> {
        for (index, limb) in value.iter().copied().enumerate() {
            let target = offset.checked_add(index)?;
            *self.0.get_mut(target)? ^= limb;
        }
        Some(())
    }

    #[cfg(test)]
    const fn bit(&self, index: usize) -> Option<bool> {
        if index < 512 {
            Some(self.0[index / 64] >> (index % 64) & 1 != 0)
        } else {
            None
        }
    }

    #[cfg(test)]
    const fn toggle_bit(&mut self, index: usize) -> Option<()> {
        if index < 512 {
            self.0[index / 64] ^= 1_u64 << (index % 64);
            Some(())
        } else {
            None
        }
    }
}

/// Carry-less 64×64 product, represented as two little-endian limbs.
fn clmul64(a: u64, b: u64) -> [u64; 2] {
    crate::gf64::clmul_raw(a, b)
}

/// Four-limb Karatsuba product. Three recursive 128-bit products require
/// nine carry-less 64×64 products rather than sixteen schoolbook products.
fn clmul256(a: [u64; 4], b: [u64; 4]) -> Wide {
    let z0 = clmul128([a[0], a[1]], [b[0], b[1]]);
    let z2 = clmul128([a[2], a[3]], [b[2], b[3]]);
    let z1 = clmul128([a[0] ^ a[2], a[1] ^ a[3]], [b[0] ^ b[2], b[1] ^ b[3]]);
    let mut middle = z1;
    for index in 0..4 {
        middle[index] ^= z0[index] ^ z2[index];
    }
    let mut product = Wide([0; 8]);
    product.xor_at(0, &z0).expect("low product fits in 8 limbs");
    product
        .xor_at(2, &middle)
        .expect("middle product fits in 8 limbs");
    product
        .xor_at(4, &z2)
        .expect("high product fits in 8 limbs");
    product
}

fn clmul128(a: [u64; 2], b: [u64; 2]) -> [u64; 4] {
    let z0 = clmul64(a[0], b[0]);
    let z2 = clmul64(a[1], b[1]);
    let z1 = clmul64(a[0] ^ a[1], b[0] ^ b[1]);
    [
        z0[0],
        z0[1] ^ z1[0] ^ z0[0] ^ z2[0],
        z2[0] ^ z1[1] ^ z0[1] ^ z2[1],
        z2[1],
    ]
}

fn xor_shifted_into_320(target: &mut [u64; 5], source: &[u64; 4], shift: usize) -> Option<()> {
    let word_offset = shift / 64;
    let bit_offset = shift % 64;
    let shift_amount = u32::try_from(bit_offset).ok()?;
    for (index, limb) in source.iter().copied().enumerate() {
        let target_index = index.checked_add(word_offset)?;
        *target.get_mut(target_index)? ^= limb.wrapping_shl(shift_amount);
        if bit_offset != 0 {
            let spill_index = target_index.checked_add(1)?;
            *target.get_mut(spill_index)? ^= limb >> (64_usize.checked_sub(bit_offset)?);
        }
    }
    Some(())
}

fn reduce(product: Wide) -> [u64; 4] {
    // Fold the high half with x^256 = 1 + x^2 + x^5 + x^10. The shifted
    // terms spill at most ten bits above degree 255; fold that tail once more,
    // which stays below degree 20.
    let high = [product.0[4], product.0[5], product.0[6], product.0[7]];
    let mut folded = [0_u64; 5];
    for shift in [0, 2, 5, 10] {
        xor_shifted_into_320(&mut folded, &high, shift)
            .expect("a four-limb value shifted by at most ten fits in 320 bits");
    }
    let tail = folded[4];
    let tail_reduced = tail ^ tail.wrapping_shl(2) ^ tail.wrapping_shl(5) ^ tail.wrapping_shl(10);
    [
        product.0[0] ^ folded[0] ^ tail_reduced,
        product.0[1] ^ folded[1],
        product.0[2] ^ folded[2],
        product.0[3] ^ folded[3],
    ]
}

const fn spread_u16(mut value: u32) -> u32 {
    value = (value | value.wrapping_shl(8)) & 0x00ff_00ff;
    value = (value | value.wrapping_shl(4)) & 0x0f0f_0f0f;
    value = (value | value.wrapping_shl(2)) & 0x3333_3333;
    (value | value.wrapping_shl(1)) & 0x5555_5555
}

impl BinaryField for Gf256 {
    const BITS: usize = 256;
    const BYTES: usize = 32;
    type Bytes = [u8; 32];
    const ZERO: Self = Self([0; 4]);
    const ONE: Self = Self([1, 0, 0, 0]);

    fn add(self, other: Self) -> Self {
        Self([
            self.0[0] ^ other.0[0],
            self.0[1] ^ other.0[1],
            self.0[2] ^ other.0[2],
            self.0[3] ^ other.0[3],
        ])
    }

    fn mul(self, other: Self) -> Self {
        Self(reduce(clmul256(self.0, other.0)))
    }

    fn square(self) -> Self {
        let mut wide = Wide([0; 8]);
        for (limb_index, limb) in self.0.into_iter().enumerate() {
            let mut spread = 0_u128;
            for (chunk_index, chunk) in limb.to_le_bytes().chunks_exact(2).enumerate() {
                let value = u16::from_le_bytes(chunk.try_into().expect("two-byte chunk"));
                let shift = chunk_index
                    .checked_mul(32)
                    .expect("four chunks fit in 128 bits");
                spread |= u128::from(spread_u16(u32::from(value))) << shift;
            }
            let product_word = limb_index
                .checked_mul(2)
                .expect("square output has two limbs per input limb");
            let bytes = spread.to_le_bytes();
            let low = u64::from_le_bytes(bytes[..8].try_into().expect("low 64-bit half"));
            let high = u64::from_le_bytes(bytes[8..].try_into().expect("high 64-bit half"));
            wide.0[product_word] = low;
            let high_word = product_word
                .checked_add(1)
                .expect("square output has two limbs per input limb");
            wide.0[high_word] = high;
        }
        Self(reduce(wide))
    }

    fn bit(index: usize) -> Option<Self> {
        (index < Self::BITS).then(|| {
            let mut limbs = [0; 4];
            limbs[index / 64] = 1_u64 << (index % 64);
            Self(limbs)
        })
    }

    fn from_u64(value: u64) -> Self {
        Self([value, 0, 0, 0])
    }

    fn to_le_bytes(self) -> Self::Bytes {
        let mut bytes = [0; 32];
        for (limb, chunk) in self.0.into_iter().zip(bytes.chunks_exact_mut(8)) {
            chunk.copy_from_slice(&limb.to_le_bytes());
        }
        bytes
    }

    fn from_le_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::BYTES {
            return None;
        }
        let mut limbs = [0; 4];
        for (limb, chunk) in limbs.iter_mut().zip(bytes.chunks_exact(8)) {
            *limb = u64::from_le_bytes(chunk.try_into().ok()?);
        }
        Some(Self(limbs))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::{Gf256, Gf256Decoder, Wide};
    use crate::binary_field::BinaryField;
    use alloc::{vec, vec::Vec};

    fn reference_product(a: Gf256, b: Gf256) -> Gf256 {
        let mut product = Wide([0; 8]);
        for bit in 0..256 {
            if b.test_bit(bit) {
                for source in 0..256 {
                    if a.test_bit(source) {
                        let index = source
                            .checked_add(bit)
                            .expect("product degree is below 512");
                        product.toggle_bit(index).expect("product bit is below 512");
                    }
                }
            }
        }
        // Independent long division by x^256 + x^10 + x^5 + x^2 + 1.
        for bit in (256..512).rev() {
            if product
                .bit(bit)
                .expect("reduction scans the 512-bit product")
            {
                let shift = bit
                    .checked_sub(256)
                    .expect("high product bit is at least 256");
                for exponent in [0, 2, 5, 10, 256] {
                    let index = shift
                        .checked_add(exponent)
                        .expect("polynomial degree is below 512");
                    product
                        .toggle_bit(index)
                        .expect("reduction bit is below 512");
                }
            }
        }
        Gf256([product.0[0], product.0[1], product.0[2], product.0[3]])
    }

    #[test]
    fn carryless_products_match_bitwise_reference() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..32 {
            let mut a = [0; 4];
            let mut b = [0; 4];
            for limb in &mut a {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *limb = state;
            }
            for limb in &mut b {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *limb = state;
            }
            let left = Gf256::from_limbs(a);
            let right = Gf256::from_limbs(b);
            assert_eq!(left.mul(right), reference_product(left, right));
            assert_eq!(left.square(), left.mul(left));
        }
    }

    #[test]
    fn byte_mapping_is_little_endian_and_round_trips() {
        let value = Gf256::from_limbs([0x0123_4567_89ab_cdef, 2, 3, 4]);
        let bytes = value.to_le_bytes();
        assert_eq!(&bytes[..8], &0x0123_4567_89ab_cdef_u64.to_le_bytes());
        assert_eq!(Gf256::from_le_bytes(&bytes), Some(value));
        assert!(Gf256::from_le_bytes(&bytes[..31]).is_none());
    }

    #[test]
    fn multiplication_obeys_field_identities() {
        let a = Gf256::from_limbs([u64::MAX, 0, 0x1234, 0xfeed]);
        let b = Gf256::from_limbs([5, 9, 11, 13]);
        assert_eq!(a.mul(Gf256::ONE), a);
        assert_eq!(a.mul(Gf256::ZERO), Gf256::ZERO);
        assert_eq!(a.mul(b), b.mul(a));
        assert_eq!(a.mul(b.add(Gf256::ONE)), a.mul(b).add(a));
    }

    #[test]
    fn reusable_decoder_decodes_the_same_set_across_calls() {
        let decoder = Gf256Decoder::build().expect("the fixed quadratic map is solvable");
        let odd = [
            Gf256::from_limbs([3, 0, 0, 0]),
            Gf256::from_limbs([9, 0, 0, 0]),
        ];
        let expected = vec![
            Gf256::from_limbs([1, 0, 0, 0]),
            Gf256::from_limbs([2, 0, 0, 0]),
        ];
        assert_eq!(decoder.decode(&odd, expected.len()), Ok(expected.clone()));
        assert_eq!(decoder.decode(&odd, expected.len()), Ok(expected));
    }

    #[test]
    #[ignore = "full degree-256 GF(2^256) decode; run in release mode"]
    fn decodes_256_consecutive_roots_within_work_ceiling() {
        let expected: Vec<Gf256> = (1_u64..=256).map(Gf256::from_u64).collect();
        let mut odd = vec![Gf256::ZERO; expected.len()];
        for root in &expected {
            let squared = root.square();
            let mut power = *root;
            for syndrome in &mut odd {
                *syndrome = syndrome.add(power);
                power = power.mul(squared);
            }
        }
        assert_eq!(Gf256::decode(&odd, expected.len()), Ok(expected));
    }

    #[cfg(feature = "std")]
    #[test]
    #[ignore = "timing probe"]
    fn gf256_multiply_timing() {
        use std::hint::black_box;
        use std::time::Instant;

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
        let rounds = 100_000;
        let started = Instant::now();
        for _ in 0..rounds {
            let _ = black_box(black_box(left) * black_box(right));
        }
        let multiply_elapsed = started.elapsed();
        let started = Instant::now();
        for _ in 0..rounds {
            let _ = black_box(black_box(left).square());
        }
        let square_elapsed = started.elapsed();
        std::println!(
            "GF(2^256): multiply {:?}/op, square {:?}/op",
            multiply_elapsed / rounds,
            square_elapsed / rounds
        );
    }
}
