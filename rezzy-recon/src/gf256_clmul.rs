//! Fused scalar PCLMUL backend for GF(2^256) multiplication.

use std::sync::OnceLock;

use crate::gf256::{reduce_limbs, Gf256};

type MulFn = unsafe fn([u64; 4], [u64; 4]) -> Gf256;

#[allow(unsafe_code)]
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
#[allow(unsafe_code)]
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
    let mut product = [0; 8];
    for i in 0..4 {
        product[i] ^= z0[i];
        product[i + 2] ^= middle[i];
        product[i + 4] ^= z2[i];
    }
    product
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
#[allow(unsafe_code)]
fn clmul64(left: u64, right: u64) -> [u64; 2] {
    use core::arch::x86_64::{
        _mm_clmulepi64_si128, _mm_cvtsi128_si64, _mm_set_epi64x, _mm_srli_si128,
    };
    let a = _mm_set_epi64x(0, left as i64);
    let b = _mm_set_epi64x(0, right as i64);
    let product = _mm_clmulepi64_si128(a, b, 0x00);
    [
        _mm_cvtsi128_si64(product) as u64,
        _mm_cvtsi128_si64(_mm_srli_si128::<8>(product)) as u64,
    ]
}
