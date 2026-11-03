// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Shared deterministic fixtures for unit tests.

/// A deterministic, well-mixed nonzero 32-byte digest for `seed`.
pub(crate) fn digest32(seed: u64) -> [u8; 32] {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = [0_u8; 32];
    for chunk in out.chunks_exact_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes());
    }
    out
}
