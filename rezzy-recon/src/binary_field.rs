// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Binary-field abstraction for `PinSketch` decoding.
//!
//! The decoder is written against [`BinaryField`] so a wider binary field (for
//! example GF(2^256) for bucket sketches) can reuse it. `u64` is the MSC4521
//! GF(2^64) profile and must stay byte-identical to the pre-trait behavior.

/// A binary field GF(2^`BITS`): characteristic 2, so addition is XOR.
///
/// That invariant is what makes the odd-power syndrome trick and the
/// `a^(2^BITS - 2)` inverse schedule valid; do not implement this for a
/// prime field.
pub(crate) trait BinaryField: Copy + Eq {
    /// Extension degree `m` of GF(2^m).
    const BITS: usize;
    /// Serialized width in bytes, little-endian.
    const BYTES: usize = Self::BITS.div_ceil(8);
    /// Type returned by [`BinaryField::to_le_bytes`].
    type Bytes: AsRef<[u8]>;
    const ZERO: Self;
    const ONE: Self;

    fn add(self, other: Self) -> Self;
    fn mul(self, other: Self) -> Self;

    /// `self * self`. Override when a field has a cheaper dedicated squaring.
    #[inline]
    fn square(self) -> Self {
        self.mul(self)
    }

    /// Field element with only bit `index` set, or `None` when `index` is
    /// outside `0..BITS`. These are the GF(2) basis vectors used by the
    /// quadratic solver and the Frobenius trial parameters.
    fn bit(index: usize) -> Option<Self>;

    /// Embeds `value` in the low 64 bits. Profiles are at least 64 bits, so
    /// this is lossless for the deterministic trial seed.
    fn from_u64(value: u64) -> Self {
        debug_assert!(Self::BYTES >= 8, "from_u64 needs a 64-bit-wide field");
        let mut bytes = alloc::vec![0u8; Self::BYTES];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        <Self as BinaryField>::from_le_bytes(&bytes).expect("a zero-padded prefix always fits")
    }

    /// Reads bit `index` of the little-endian representation.
    fn test_bit(self, index: usize) -> bool {
        let bytes = self.to_le_bytes();
        let bytes = bytes.as_ref();
        bytes
            .get(index / 8)
            .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
    }

    fn to_le_bytes(self) -> Self::Bytes;

    /// Parses exactly [`Self::BYTES`] bytes; `None` on any other length.
    fn from_le_bytes(bytes: &[u8]) -> Option<Self>;

    /// `target[i] += term * source[i]` for every coefficient. The default is
    /// a scalar loop; a field may override it with a wider multiply-accumulate
    /// backend. `source` and `target` must have the same length.
    fn mac(term: Self, source: &[Self], target: &mut [Self]) {
        debug_assert_eq!(source.len(), target.len());
        for (coefficient, slot) in source.iter().zip(target.iter_mut()) {
            *slot = slot.add(term.mul(*coefficient));
        }
    }

    /// `self^(2^BITS - 2)` via a fixed schedule; `None` for zero.
    fn inv(self) -> Option<Self> {
        if self == Self::ZERO {
            return None;
        }
        // a^(2^m - 2) = prod_{i=1..m-1} a^(2^i).
        let mut result = Self::ONE;
        let mut square = self;
        for _ in 1..Self::BITS {
            square = square.square();
            result = result.mul(square);
        }
        Some(result)
    }
}

/// GF(2^64) in the MSC4521 representation.
impl BinaryField for u64 {
    const BITS: usize = 64;
    const BYTES: usize = 8;
    type Bytes = [u8; 8];
    const ZERO: Self = 0;
    const ONE: Self = 1;

    #[inline]
    fn add(self, other: Self) -> Self {
        self ^ other
    }

    #[inline]
    fn mul(self, other: Self) -> Self {
        crate::gf64::mul(self, other)
    }

    #[inline]
    fn bit(index: usize) -> Option<Self> {
        (index < <Self as BinaryField>::BITS).then(|| 1_u64 << index)
    }

    #[inline]
    fn to_le_bytes(self) -> Self::Bytes {
        self.to_le_bytes()
    }

    #[inline]
    fn from_le_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::BYTES {
            return None;
        }
        let bytes: [u8; 8] = bytes.try_into().ok()?;
        Some(Self::from_le_bytes(bytes))
    }

    #[inline]
    fn mac(term: Self, source: &[Self], target: &mut [Self]) {
        crate::gf64_simd::poly_mac(term, source, target);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::BinaryField;

    /// GF(2^256) reduction polynomial `x^256 + x^10 + x^5 + x^2 + 1`.
    /// Keep this test with the experimental field profile so its modulus
    /// cannot silently change after the little-endian byte mapping is pinned.
    const GF256_LOW_EXPONENTS: [usize; 4] = [10, 5, 2, 0];

    /// GF(2)[x] polynomial as a 512-bit little-endian bitset.
    type Poly = [u64; 8];

    fn bit(poly: &Poly, index: usize) -> bool {
        poly.get(index / 64)
            .is_some_and(|word| word >> (index % 64) & 1 == 1)
    }

    fn degree(poly: &Poly) -> Option<usize> {
        (0..512).rev().find(|&i| bit(poly, i))
    }

    fn xor_shifted(target: &mut Poly, source: &Poly, shift: usize) -> Option<()> {
        let source_bits = 512_usize.checked_sub(shift)?;
        for i in (0..source_bits).filter(|&i| bit(source, i)) {
            let target_bit = i.checked_add(shift)?;
            *target.get_mut(target_bit / 64)? ^= 1 << (target_bit % 64);
        }
        Some(())
    }

    fn reduce(mut value: Poly, modulus: &Poly) -> Poly {
        let modulus_degree = degree(modulus).expect("non-zero modulus");
        while let Some(value_degree) = degree(&value) {
            if value_degree < modulus_degree {
                break;
            }
            xor_shifted(
                &mut value,
                modulus,
                value_degree
                    .checked_sub(modulus_degree)
                    .expect("long division only shifts high terms down"),
            )
            .expect("shifted polynomial fits in the bitset");
        }
        value
    }

    fn mul_mod(a: &Poly, b: &Poly, modulus: &Poly) -> Poly {
        let mut product = [0_u64; 8];
        for i in (0..256).filter(|&i| bit(b, i)) {
            xor_shifted(&mut product, a, i).expect("256-bit product fits in 512 bits");
        }
        reduce(product, modulus)
    }

    fn gcd(mut a: Poly, mut b: Poly) -> Poly {
        while degree(&b).is_some() {
            let remainder = reduce(a, &b);
            a = b;
            b = remainder;
        }
        a
    }

    /// Rabin's test for degree 256 (whose only prime divisor is 2):
    /// `f` is irreducible iff `x^(2^256) = x mod f` and
    /// `gcd(x^(2^128) + x, f) = 1`.
    #[test]
    fn gf256_reduction_polynomial_is_irreducible() {
        let mut modulus = [0_u64; 8];
        modulus[4] |= 1; // x^256
        for exponent in GF256_LOW_EXPONENTS {
            modulus[exponent / 64] |= 1 << (exponent % 64);
        }
        let mut x = [0_u64; 8];
        x[0] = 2;

        let mut power = x; // x^(2^0)
        let mut midpoint = [0_u64; 8];
        for round in 1..=256 {
            power = mul_mod(&power, &power, &modulus);
            if round == 128 {
                midpoint = power;
            }
        }
        assert_eq!(power, x, "x^(2^256) != x mod f");

        let mut difference = midpoint;
        difference[0] ^= x[0];
        let mut one = [0_u64; 8];
        one[0] = 1;
        assert_eq!(
            gcd(modulus, difference),
            one,
            "f shares a factor of degree dividing 128"
        );
    }

    /// Sanity-checks the test's own arithmetic on a known reducible
    /// polynomial, so a vacuous pass is impossible.
    #[test]
    fn rabin_check_rejects_a_reducible_polynomial() {
        // x^256 + 1 = (x + 1)^256 over GF(2).
        let mut modulus = [0_u64; 8];
        modulus[4] |= 1;
        modulus[0] |= 1;
        let mut x = [0_u64; 8];
        x[0] = 2;
        let mut power = x;
        let mut midpoint = [0_u64; 8];
        for round in 1..=256 {
            power = mul_mod(&power, &power, &modulus);
            if round == 128 {
                midpoint = power;
            }
        }
        let mut difference = midpoint;
        difference[0] ^= x[0];
        let mut one = [0_u64; 8];
        one[0] = 1;
        assert!(power != x || gcd(modulus, difference) != one);
    }

    #[test]
    fn gf64_inverse_round_trips() {
        assert_eq!(0_u64.inv(), None);
        for value in [1_u64, 2, 3, 0xdead_beef, u64::MAX, 0x9e37_79b9_7f4a_7c15] {
            let inverse = value.inv().expect("non-zero");
            assert_eq!(value.mul(inverse), 1);
        }
    }
}
