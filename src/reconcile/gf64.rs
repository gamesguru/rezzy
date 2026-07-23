// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Arithmetic for the MSC0501 minisketch-compatible binary field.

/// Reduction polynomial without its implicit `x^64` term.
const REDUCTION: u64 = 0x1b;

/// Portable constant-time carry-less multiplication in
/// `GF(2)[x] / (x^64 + x^4 + x^3 + x + 1)`.
#[must_use]
pub fn mul_bitwise(mut left: u64, mut right: u64) -> u64 {
    let mut product = 0;
    for _ in 0..64 {
        product ^= left & 0_u64.wrapping_sub(right & 1);
        right >>= 1;

        let carry = left >> 63;
        left <<= 1;
        left ^= REDUCTION & 0_u64.wrapping_sub(carry);
    }
    product
}

/// Multiplies two elements of the MSC0501 64-bit binary field.
///
/// Uses the portable fallback unless a target-specific accelerated implementation is enabled.
#[must_use]
pub fn mul(left: u64, right: u64) -> u64 {
    mul_bitwise(left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduction_sensitive_vectors_match_minisketch() {
        assert_eq!(mul(0, u64::MAX), 0);
        assert_eq!(mul(1, u64::MAX), u64::MAX);
        assert_eq!(mul(0x1b, 0x1b), 0x145);
        assert_eq!(mul(u64::MAX, u64::MAX), 0x5555_5555_5555_5513);
        assert_eq!(mul(1_u64 << 63, 1_u64 << 63), 0xc000_0000_0000_005a);
    }

    #[test]
    fn multiplication_is_distributive() {
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        for _ in 0..256 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let left = state;
            state = state.rotate_left(23) ^ 0x9e37_79b9_7f4a_7c15;
            let right = state;
            state = state.rotate_right(11) ^ 0x3c6e_f372_fe94_f82b;
            let addend = state;
            assert_eq!(
                mul(left, right ^ addend),
                mul(left, right) ^ mul(left, addend)
            );
        }
    }
}
