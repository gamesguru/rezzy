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
use alloc::{vec, vec::Vec};

/// Identifier of the full-digest GF(2^256) bucket-sketch algorithm.
pub const PROFILE_ID: &str = "algebraic_gf256";

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

    /// Parses exactly 32 little-endian bytes, returning `None` for any other length.
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

    /// Decodes odd-power syndromes (`s1`, `s3`, ...) into sorted, nonzero elements.
    /// `max_elements` limits the degree of the recovered locator polynomial.
    /// Uses the current GF(2^256) work ceiling.
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
        if odd_syndromes.iter().all(|s| *s == Self::ZERO) {
            // An all-zero sketch decodes to the empty set; skip building the
            // quadratic solver.
            return Ok(alloc::vec::Vec::new());
        }
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

/// A `PinSketch` over full 256-bit digest elements.
///
/// The element set is GF(2^256) without zero. Coordinates are serialized as
/// consecutive little-endian 32-byte field elements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gf256Sketch {
    coordinates: Vec<Gf256>,
}

impl Gf256Sketch {
    /// Creates an empty sketch of the requested capacity.
    ///
    /// # Errors
    /// Returns `InvalidSketchCapacity` for zero or a capacity above 256.
    pub fn new(capacity: usize) -> Result<Self, crate::algebraic::AlgebraicError> {
        if capacity == 0 || capacity > crate::MAX_OVERFLOW_SKETCH_CAPACITY {
            return Err(crate::algebraic::AlgebraicError::InvalidSketchCapacity);
        }
        Ok(Self {
            coordinates: vec![Gf256::ZERO; capacity],
        })
    }

    /// Number of odd-power syndrome coordinates in this sketch.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.coordinates.len()
    }

    /// Odd-power syndrome coordinates.
    pub fn coordinates(&self) -> &[Gf256] {
        &self.coordinates
    }

    /// Inserts or removes one nonzero field element.
    ///
    /// # Errors
    /// Returns `ZeroSketchElement` for the reserved all-zero digest element.
    pub fn toggle(&mut self, value: Gf256) -> Result<(), crate::algebraic::AlgebraicError> {
        if value == Gf256::ZERO {
            return Err(crate::algebraic::AlgebraicError::ZeroSketchElement);
        }
        let square = value.square();
        let mut odd_power = value;
        for coordinate in &mut self.coordinates {
            *coordinate = BinaryField::add(*coordinate, odd_power);
            odd_power = BinaryField::mul(odd_power, square);
        }
        Ok(())
    }

    /// XORs a same-capacity sketch into this one.
    ///
    /// # Errors
    /// Returns `InvalidSketchLength` when capacities differ.
    pub fn xor(&mut self, other: &Self) -> Result<(), crate::algebraic::AlgebraicError> {
        if self.capacity() != other.capacity() {
            return Err(crate::algebraic::AlgebraicError::InvalidSketchLength);
        }
        for (left, right) in self.coordinates.iter_mut().zip(&other.coordinates) {
            *left = BinaryField::add(*left, *right);
        }
        Ok(())
    }

    /// XORs the first `capacity` coordinates of `other` into this sketch.
    ///
    /// This is the incremental-cache path: a leaf sketch is maintained at the
    /// maximum normal bucket capacity, while a request may ask for a smaller
    /// prefix of the same odd-power coordinates.
    ///
    /// # Errors
    /// Returns `InvalidSketchCapacity` for zero, or `InvalidSketchLength` when
    /// `capacity` exceeds either sketch's capacity.
    pub(crate) fn xor_prefix(
        &mut self,
        other: &Self,
        capacity: usize,
    ) -> Result<(), crate::algebraic::AlgebraicError> {
        if capacity == 0 {
            return Err(crate::algebraic::AlgebraicError::InvalidSketchCapacity);
        }
        if capacity > self.capacity() || capacity > other.capacity() {
            return Err(crate::algebraic::AlgebraicError::InvalidSketchLength);
        }
        for (left, right) in self.coordinates[..capacity]
            .iter_mut()
            .zip(&other.coordinates[..capacity])
        {
            *left = BinaryField::add(*left, *right);
        }
        Ok(())
    }

    /// Returns the symmetric-difference sketch of two equal-capacity sets.
    ///
    /// # Errors
    /// Returns `InvalidSketchLength` when capacities differ.
    pub fn subtract(&self, other: &Self) -> Result<Self, crate::algebraic::AlgebraicError> {
        let mut residual = self.clone();
        residual.xor(other)?;
        Ok(residual)
    }

    /// Decodes the set difference represented by this sketch and re-encodes
    /// every coordinate to verify the result.
    ///
    /// # Errors
    /// Returns the decoder's error when the difference exceeds the requested
    /// bound, is malformed, or exceeds its work budget.
    pub fn decode_elements(
        &self,
        max_elements: usize,
        decoder: &Gf256Decoder,
    ) -> Result<Vec<Gf256>, crate::algebraic::AlgebraicError> {
        let mut budget = Gf256Decoder::per_sketch_work_ceiling();
        self.decode_elements_with_budget(max_elements, decoder, &mut budget)
    }

    /// Like [`Self::decode_elements`], but draws factoring work from `budget`
    /// and leaves the unspent remainder in it.
    ///
    /// # Errors
    /// Same as [`Self::decode_elements`].
    pub(crate) fn decode_elements_with_budget(
        &self,
        max_elements: usize,
        decoder: &Gf256Decoder,
        budget: &mut usize,
    ) -> Result<Vec<Gf256>, crate::algebraic::AlgebraicError> {
        if max_elements == 0 || max_elements > self.capacity() {
            return Err(crate::algebraic::AlgebraicError::InvalidSketchCapacity);
        }
        if self.coordinates.iter().all(|value| *value == Gf256::ZERO) {
            return Ok(Vec::new());
        }
        let roots =
            decoder.decode_with_budget(&self.coordinates[..max_elements], max_elements, budget)?;
        if roots.len() > self.capacity() || roots.contains(&Gf256::ZERO) {
            return Err(crate::algebraic::AlgebraicError::DecodeFailure);
        }
        let mut check = Self::new(self.capacity())?;
        for root in roots.iter().copied() {
            check.toggle(root)?;
        }
        if check == *self {
            Ok(roots)
        } else {
            Err(crate::algebraic::AlgebraicError::DecodeFailure)
        }
    }

    /// Decodes the digest bytes represented by this sketch.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::decode_elements`].
    pub fn decode_digests(
        &self,
        max_elements: usize,
        decoder: &Gf256Decoder,
    ) -> Result<Vec<[u8; 32]>, crate::algebraic::AlgebraicError> {
        self.decode_elements(max_elements, decoder)
            .map(|elements| elements.into_iter().map(Gf256::to_le_bytes).collect())
    }

    /// Extracts the exact symmetric difference between two same-capacity
    /// sketches as their original 32-byte digest values.
    ///
    /// Membership in the local set can then distinguish remote-only from
    /// local-only elements.
    ///
    /// # Errors
    /// Returns an error for unequal capacities or an undecodable residual.
    pub fn extract_symmetric_difference(
        local: &Self,
        remote: &Self,
        max_elements: usize,
        decoder: &Gf256Decoder,
    ) -> Result<Vec<[u8; 32]>, crate::algebraic::AlgebraicError> {
        local
            .subtract(remote)?
            .decode_digests(max_elements, decoder)
    }

    /// Serializes the coordinate payload as little-endian 32-byte elements.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.capacity().saturating_mul(32));
        for coordinate in &self.coordinates {
            bytes.extend_from_slice(&coordinate.to_le_bytes());
        }
        bytes
    }

    /// Parses a coordinate payload for a sketch of the given `capacity`.
    ///
    /// # Errors
    /// Returns an error for zero/excessive capacity or a non-exact payload size.
    pub fn from_bytes(
        capacity: usize,
        bytes: &[u8],
    ) -> Result<Self, crate::algebraic::AlgebraicError> {
        let mut sketch = Self::new(capacity)?;
        let expected = capacity
            .checked_mul(32)
            .ok_or(crate::algebraic::AlgebraicError::InvalidSketchLength)?;
        if bytes.len() != expected {
            return Err(crate::algebraic::AlgebraicError::InvalidSketchLength);
        }
        for (coordinate, chunk) in sketch.coordinates.iter_mut().zip(bytes.chunks_exact(32)) {
            *coordinate = Gf256::from_le_bytes(chunk)
                .ok_or(crate::algebraic::AlgebraicError::InvalidSketchLength)?;
        }
        Ok(sketch)
    }
}

impl Gf256Decoder {
    /// Per-sketch work ceiling: the budget one sketch gets from a fresh
    /// [`Self::decode`] call.
    #[must_use]
    pub(crate) const fn per_sketch_work_ceiling() -> usize {
        crate::pinsketch::WorkModel::GF256.max_factor_work()
    }

    /// Like [`Self::decode`], but draws factoring work from `budget` and
    /// leaves the unspent remainder in it.
    ///
    /// # Errors
    /// Same as [`Self::decode`]; `BudgetExhausted` means `budget` ran out.
    pub(crate) fn decode_with_budget(
        &self,
        odd_syndromes: &[Gf256],
        max_elements: usize,
        budget: &mut usize,
    ) -> Result<Vec<Gf256>, crate::algebraic::AlgebraicError> {
        crate::pinsketch::decode_with_model_budget(
            odd_syndromes,
            max_elements,
            crate::pinsketch::WorkModel::GF256,
            Some(&self.quadratic_solver),
            budget,
        )
    }

    /// Builds the quadratic solver once for reuse by future decodes.
    #[must_use]
    pub fn build() -> Option<Self> {
        Some(Self {
            quadratic_solver: QuadraticSolver::new()?,
        })
    }

    /// Returns a process-wide decoder, building it at most once.
    ///
    /// The quadratic solver is field-wide and immutable, so the build cost
    /// (a full 256x256 elimination) only needs paying once. This mirrors the
    /// cached GF(2^64) solver used by the root-finding path.
    #[cfg(feature = "std")]
    #[must_use]
    pub fn cached() -> Option<&'static Self> {
        static DECODER: std::sync::OnceLock<Option<Gf256Decoder>> = std::sync::OnceLock::new();
        DECODER.get_or_init(Self::build).as_ref()
    }

    /// Decodes odd-power syndromes (`s1`, `s3`, ...) into sorted, nonzero elements.
    /// `max_elements` limits the degree of the recovered locator polynomial.
    /// Uses this decoder's cached quadratic solver.
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
/// Returns the process-wide cached quadratic solver for GF(2^256).
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

#[cfg(all(feature = "std", target_arch = "x86_64"))]
/// Reduces eight 64-bit limbs modulo the field polynomial.
pub(crate) fn reduce_limbs(limbs: [u64; 8]) -> Gf256 {
    Gf256(reduce(Wide(limbs)))
}

fn reduce(product: Wide) -> [u64; 4] {
    // Fold the high half with x^256 = 1 + x^2 + x^5 + x^10. A shift by `k`
    // spills the top `k` bits of the last high limb above degree 255; those
    // collect in `spill` (below degree 10) and fold once more into limb 0.
    let [l0, l1, l2, l3, h0, h1, h2, h3] = product.0;
    let mut r = [l0 ^ h0, l1 ^ h1, l2 ^ h2, l3 ^ h3];
    let mut spill = 0_u64;
    for (left, right) in [(2_u32, 62_u32), (5, 59), (10, 54)] {
        r[0] ^= h0 << left;
        r[1] ^= (h1 << left) | (h0 >> right);
        r[2] ^= (h2 << left) | (h1 >> right);
        r[3] ^= (h3 << left) | (h2 >> right);
        spill ^= h3 >> right;
    }
    r[0] ^= spill ^ (spill << 2) ^ (spill << 5) ^ (spill << 10);
    r
}

/// Interleaves the low 32 bits of `value` with zeros: bit `i` moves to bit
/// `2i`. The caller guarantees `value < 2^32`.
const fn spread_u32(value: u64) -> u64 {
    let mut v = value;
    v = (v | (v << 16)) & 0x0000_ffff_0000_ffff;
    v = (v | (v << 8)) & 0x00ff_00ff_00ff_00ff;
    v = (v | (v << 4)) & 0x0f0f_0f0f_0f0f_0f0f;
    v = (v | (v << 2)) & 0x3333_3333_3333_3333;
    (v | (v << 1)) & 0x5555_5555_5555_5555
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
        // Squaring is GF(2)-linear: bit `i` of the input lands on bit `2i`.
        let [a0, a1, a2, a3] = self.0;
        Self(reduce(Wide([
            spread_u32(a0 & 0xffff_ffff),
            spread_u32(a0 >> 32),
            spread_u32(a1 & 0xffff_ffff),
            spread_u32(a1 >> 32),
            spread_u32(a2 & 0xffff_ffff),
            spread_u32(a2 >> 32),
            spread_u32(a3 & 0xffff_ffff),
            spread_u32(a3 >> 32),
        ])))
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

    /// Dispatches to the lane-parallel backend when it is compiled in and the
    /// CPU supports AVX-512 VPCLMULQDQ; otherwise uses the scalar fallback.
    /// Non-x86_64/`no_std` builds do not compile the vector path.
    fn mac(term: Self, source: &[Self], target: &mut [Self]) {
        crate::gf256_simd::poly_mac(term, source, target);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::{Gf256, Gf256Decoder, Gf256Sketch, Wide};
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
    fn full_digest_sketch_round_trips_and_extracts_symmetric_difference() {
        let decoder = Gf256Decoder::build().expect("the fixed quadratic map is solvable");
        let mut local = Gf256Sketch::new(4).unwrap();
        let mut remote = Gf256Sketch::new(4).unwrap();
        let one = Gf256::from_limbs([1, 0, 0, 0]);
        let two = Gf256::from_limbs([2, 0, 0, 0]);
        let three = Gf256::from_limbs([3, 0, 0, 0]);
        local.toggle(one).unwrap();
        local.toggle(two).unwrap();
        remote.toggle(two).unwrap();
        remote.toggle(three).unwrap();

        let payload = remote.to_bytes();
        let remote = Gf256Sketch::from_bytes(4, &payload).unwrap();
        let mut expected = vec![one.to_le_bytes(), three.to_le_bytes()];
        expected.sort_unstable();
        let mut extracted =
            Gf256Sketch::extract_symmetric_difference(&local, &remote, 4, &decoder).unwrap();
        extracted.sort_unstable();
        assert_eq!(extracted, expected);
        assert_eq!(payload.len(), 4 * 32);
        assert_eq!(Gf256Sketch::from_bytes(4, &payload).unwrap(), remote);
    }

    #[test]
    fn full_digest_sketch_rejects_zero_element_and_wrong_payload_size() {
        let mut sketch = Gf256Sketch::new(1).unwrap();
        assert_eq!(
            sketch.toggle(Gf256::ZERO),
            Err(crate::algebraic::AlgebraicError::ZeroSketchElement)
        );
        assert_eq!(
            Gf256Sketch::from_bytes(1, &[0; 31]),
            Err(crate::algebraic::AlgebraicError::InvalidSketchLength)
        );
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
