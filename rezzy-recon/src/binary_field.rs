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
        <Self as BinaryField>::from_le_bytes(&value.to_le_bytes())
            .expect("an eight-byte prefix always fits")
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
            square = square.mul(square);
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

    #[test]
    fn gf64_inverse_round_trips() {
        assert_eq!(0_u64.inv(), None);
        for value in [1_u64, 2, 3, 0xdead_beef, u64::MAX, 0x9e37_79b9_7f4a_7c15] {
            let inverse = value.inv().expect("non-zero");
            assert_eq!(value.mul(inverse), 1);
        }
    }
}
