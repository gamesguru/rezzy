//! Fused scalar PCLMUL backend for GF(2^256) multiplication.

use std::sync::OnceLock;

use crate::gf256::{reduce_limbs, Gf256};

type MulFn = unsafe fn([u64; 4], [u64; 4]) -> Gf256;

#[allow(unsafe_code)] // calls the cached target-feature function after runtime detection
pub(crate) fn try_mul(left: Gf256, right: Gf256) -> Option<Gf256> {
    let implementation = implementation()?;
    // SAFETY: the cached function exists only after PCLMUL support was detected.
    Some(unsafe { implementation(left.limbs(), right.limbs()) })
}

fn implementation() -> Option<MulFn> {
    static IMPLEMENTATION: OnceLock<Option<MulFn>> = OnceLock::new();
    *IMPLEMENTATION.get_or_init(|| std::is_x86_feature_detected!("pclmulqdq").then_some(mul_pclmul))
}

#[target_feature(enable = "pclmulqdq")]
#[allow(unsafe_code)] // declares an `unsafe fn` that requires PCLMULQDQ
unsafe fn mul_pclmul(left: [u64; 4], right: [u64; 4]) -> Gf256 {
    let limbs = clmul256_pclmul(left, right);
    reduce_limbs(limbs)
}

#[target_feature(enable = "pclmulqdq")]
fn clmul256_pclmul(a: [u64; 4], b: [u64; 4]) -> [u64; 8] {
    let z0 = clmul128([a[0], a[1]], [b[0], b[1]]);
    let z2 = clmul128([a[2], a[3]], [b[2], b[3]]);
    let z1 = clmul128([a[0] ^ a[2], a[1] ^ a[3]], [b[0] ^ b[2], b[1] ^ b[3]]);
    let mut middle = z1;
    for i in 0..4 {
        middle[i] ^= z0[i] ^ z2[i];
    }
    // product = z0 + (middle << 128) + (z2 << 256), as 64-bit limbs.
    [
        z0[0],
        z0[1],
        z0[2] ^ middle[0],
        z0[3] ^ middle[1],
        z2[0] ^ middle[2],
        z2[1] ^ middle[3],
        z2[2],
        z2[3],
    ]
}

#[target_feature(enable = "pclmulqdq")]
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

#[target_feature(enable = "pclmulqdq")]
#[inline]
fn clmul64(left: u64, right: u64) -> [u64; 2] {
    use core::arch::x86_64::{
        _mm_clmulepi64_si128, _mm_cvtsi128_si64, _mm_set_epi64x, _mm_srli_si128,
    };
    // Bit-for-bit reinterpretation between u64 and i64 (no `as` casts).
    let a = _mm_set_epi64x(0, i64::from_ne_bytes(left.to_ne_bytes()));
    let b = _mm_set_epi64x(0, i64::from_ne_bytes(right.to_ne_bytes()));
    let product = _mm_clmulepi64_si128(a, b, 0x00);
    [
        u64::from_ne_bytes(_mm_cvtsi128_si64(product).to_ne_bytes()),
        u64::from_ne_bytes(_mm_cvtsi128_si64(_mm_srli_si128::<8>(product)).to_ne_bytes()),
    ]
}
