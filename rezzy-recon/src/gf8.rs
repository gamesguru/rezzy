// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Arithmetic in GF(2^8), the 8-bit field used by Reed-Solomon codes.
//!
//! The field is `GF(2)[x] / (x^8 + x^4 + x^3 + x^2 + 1)` (`0x11d`), with
//! `0x02` (`x`) as a primitive element. Addition and subtraction are XOR.
//! This is distinct from [`crate::gf256`], which is GF(2^256): "gf8" here
//! means eight-bit symbols, not an eight-element field.

use crate::binary_field::BinaryField;

/// Reduction polynomial without its implicit `x^8` term.
const REDUCTION: u8 = 0x1d;

const EXP: [u8; 512] = build_exp();
const LOG: [u8; 256] = build_log(&EXP);

#[allow(clippy::arithmetic_side_effects)]
const fn build_exp() -> [u8; 512] {
    let mut table = [0_u8; 512];
    let mut value = 1_u8;
    let mut index = 0;
    while index < 255 {
        table[index] = value;
        let carry = value & 0x80;
        value <<= 1;
        if carry != 0 {
            value ^= REDUCTION;
        }
        index += 1;
    }
    // Mirror the cycle so `EXP[LOG[a] + LOG[b]]` never needs a modulo.
    let mut repeat = 0;
    while repeat < 255 {
        table[255 + repeat] = table[repeat];
        repeat += 1;
    }
    table
}

#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
const fn build_log(exp: &[u8; 512]) -> [u8; 256] {
    let mut table = [0_u8; 256];
    let mut index = 0;
    while index < 255 {
        table[exp[index] as usize] = index as u8;
        index += 1;
    }
    table
}

/// An element of GF(2^8).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[must_use]
pub struct Gf8(u8);

impl Gf8 {
    /// The additive identity.
    pub const ZERO: Self = Self(0);
    /// The multiplicative identity.
    pub const ONE: Self = Self(1);
    /// The primitive element `x` (`0x02`), which generates every nonzero
    /// element as a power.
    pub const GENERATOR: Self = Self(2);

    /// Constructs an element from its eight-bit polynomial representation.
    pub const fn from_u8(value: u8) -> Self {
        Self(value)
    }

    /// Returns the eight-bit polynomial representation.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        self.0
    }

    /// Returns whether this is the additive identity.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Field addition, which is XOR.
    pub const fn add(self, other: Self) -> Self {
        Self(self.0 ^ other.0)
    }

    /// Field subtraction, which is the same as addition in characteristic 2.
    pub const fn sub(self, other: Self) -> Self {
        self.add(other)
    }

    /// Field multiplication.
    #[allow(clippy::arithmetic_side_effects, clippy::should_implement_trait)]
    pub fn mul(self, other: Self) -> Self {
        if self.0 == 0 || other.0 == 0 {
            return Self::ZERO;
        }
        let index = usize::from(LOG[usize::from(self.0)]) + usize::from(LOG[usize::from(other.0)]);
        Self(EXP[index])
    }

    /// Squares this element.
    pub fn square(self) -> Self {
        self.mul(self)
    }

    /// Returns the multiplicative inverse, or `None` for zero.
    #[must_use = "the inverse is discarded"]
    #[allow(clippy::arithmetic_side_effects)]
    pub fn inv(self) -> Option<Self> {
        if self.0 == 0 {
            return None;
        }
        let index = 255 - usize::from(LOG[usize::from(self.0)]);
        Some(Self(EXP[index]))
    }

    /// Divides this element by `other`, or returns `None` when `other` is zero.
    #[must_use = "the quotient is discarded"]
    #[allow(clippy::should_implement_trait)]
    pub fn div(self, other: Self) -> Option<Self> {
        other.inv().map(|inverse| self.mul(inverse))
    }

    /// Raises this element to `exponent`.
    ///
    /// `Gf8::ZERO.pow(0)` is [`Self::ONE`], matching the usual field
    /// convention for an empty product.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn pow(self, exponent: u32) -> Self {
        if exponent == 0 {
            return Self::ONE;
        }
        if self.0 == 0 {
            return Self::ZERO;
        }
        let logarithm = u32::from(LOG[usize::from(self.0)]);
        let index = usize::try_from((logarithm * (exponent % 255)) % 255).unwrap_or_default();
        Self(EXP[index])
    }

    /// Returns the discrete logarithm of this element in base
    /// [`Self::GENERATOR`], or `None` for zero.
    #[must_use = "the logarithm is discarded"]
    pub fn log(self) -> Option<u8> {
        (self.0 != 0).then(|| LOG[usize::from(self.0)])
    }

    /// Returns [`Self::GENERATOR`] raised to `exponent`.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn exp(exponent: u32) -> Self {
        let index = usize::try_from(exponent % 255).unwrap_or_default();
        Self(EXP[index])
    }
}

impl From<u8> for Gf8 {
    fn from(value: u8) -> Self {
        Self(value)
    }
}

impl From<Gf8> for u8 {
    fn from(value: Gf8) -> Self {
        value.0
    }
}

// `Self::add`/`Self::mul` would resolve to the trait methods being defined
// here, so the inherent functions must be named explicitly.
#[allow(clippy::use_self)]
impl BinaryField for Gf8 {
    const BITS: usize = 8;
    const BYTES: usize = 1;
    type Bytes = [u8; 1];
    const ZERO: Self = Self::ZERO;
    const ONE: Self = Self::ONE;

    fn add(self, other: Self) -> Self {
        Gf8::add(self, other)
    }

    fn mul(self, other: Self) -> Self {
        Gf8::mul(self, other)
    }

    fn bit(index: usize) -> Option<Self> {
        (index < <Self as BinaryField>::BITS).then(|| Self(1_u8 << index))
    }

    fn from_u64(value: u64) -> Self {
        Self(u8::try_from(value & 0xff).expect("masked to eight bits"))
    }

    fn to_le_bytes(self) -> Self::Bytes {
        [self.0]
    }

    fn from_le_bytes(bytes: &[u8]) -> Option<Self> {
        (bytes.len() == 1).then(|| Self(bytes[0]))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Independent bit-by-bit reference using the same reduction polynomial.
    fn reference_mul(mut left: u8, mut right: u8) -> u8 {
        let mut product = 0_u8;
        for _ in 0..8 {
            if right & 1 != 0 {
                product ^= left;
            }
            let carry = left & 0x80;
            left <<= 1;
            if carry != 0 {
                left ^= REDUCTION;
            }
            right >>= 1;
        }
        product
    }

    #[test]
    fn multiplication_matches_the_bitwise_reference() {
        for left in 0..=u8::MAX {
            for right in 0..=u8::MAX {
                assert_eq!(
                    Gf8::from_u8(left).mul(Gf8::from_u8(right)).to_u8(),
                    reference_mul(left, right),
                    "{left:#04x} * {right:#04x}"
                );
            }
        }
    }

    #[test]
    fn addition_is_xor_and_subtraction_matches_addition() {
        for left in 0..=u8::MAX {
            for right in 0..=u8::MAX {
                let left = Gf8::from_u8(left);
                let right = Gf8::from_u8(right);
                assert_eq!(left.add(right).to_u8(), left.to_u8() ^ right.to_u8());
                assert_eq!(left.sub(right), left.add(right));
            }
        }
    }

    #[test]
    fn every_nonzero_element_has_an_inverse() {
        for value in 1..=u8::MAX {
            let value = Gf8::from_u8(value);
            let inverse = value.inv().expect("nonzero elements are invertible");
            assert_eq!(value.mul(inverse), Gf8::ONE);
            assert_eq!(value.div(value), Some(Gf8::ONE));
        }
        assert_eq!(Gf8::ZERO.inv(), None);
        assert_eq!(Gf8::ONE.inv(), Some(Gf8::ONE));
    }

    #[test]
    fn generator_has_order_255() {
        let mut seen = [false; 256];
        for exponent in 0..255 {
            let value = Gf8::exp(exponent).to_u8();
            assert_ne!(value, 0);
            assert!(!seen[usize::from(value)], "exponent {exponent} repeated");
            seen[usize::from(value)] = true;
        }
        assert!(seen[1..].iter().all(|&present| present));
        assert_eq!(Gf8::exp(255), Gf8::ONE);
    }

    #[test]
    fn pow_matches_repeated_multiplication() {
        for base in 0..=u8::MAX {
            let base = Gf8::from_u8(base);
            let mut expected = Gf8::ONE;
            for exponent in 0..16 {
                assert_eq!(base.pow(exponent), expected);
                expected = expected.mul(base);
            }
        }
    }

    #[test]
    fn log_and_exp_round_trip() {
        for value in 1..=u8::MAX {
            let value = Gf8::from_u8(value);
            let logarithm = value.log().expect("nonzero values have a logarithm");
            assert_eq!(Gf8::exp(u32::from(logarithm)), value);
        }
        assert_eq!(Gf8::ZERO.log(), None);
    }
}
