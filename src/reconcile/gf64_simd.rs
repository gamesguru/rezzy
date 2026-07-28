#![allow(unsafe_code)]

#[cfg(all(target_arch = "x86_64", has_avx512_support))]
use core::arch::x86_64::{
    __m512i, _mm512_and_si512, _mm512_bsrli_epi128, _mm512_clmulepi64_epi128, _mm512_set_epi64,
    _mm512_slli_epi64, _mm512_store_si512, _mm512_xor_si512,
};

#[cfg(all(target_arch = "x86_64", has_avx512_support))]
#[repr(align(64))]
struct AlignedArray([u64; 8]);

/// Evaluates polynomial roots in parallel blocks of 8.
pub trait Gf64Evaluator {
    /// Multiplies a constant `term` by all coefficients in `source`,
    /// and XOR-adds the results into the corresponding positions in `target`.
    /// `source` and `target` must have the same length.
    fn poly_mac(term: u64, source: &[u64], target: &mut [u64]);
}

#[derive(Clone, Copy)]
pub struct ScalarEvaluator;

impl Gf64Evaluator for ScalarEvaluator {
    fn poly_mac(term: u64, source: &[u64], target: &mut [u64]) {
        for (i, &coefficient) in source.iter().enumerate() {
            target[i] ^= crate::reconcile::gf64::mul(term, coefficient);
        }
    }
}

#[cfg(all(target_arch = "x86_64", has_avx512_support))]
#[derive(Clone, Copy)]
pub struct Avx512Evaluator;

#[cfg(all(target_arch = "x86_64", has_avx512_support))]
impl Gf64Evaluator for Avx512Evaluator {
    #[allow(clippy::incompatible_msrv)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn poly_mac(term: u64, source: &[u64], target: &mut [u64]) {
        assert_eq!(source.len(), target.len());
        let mut i = 0;
        let len = source.len();

        // SAFETY: The `get_evaluator` dispatcher ensures this function is only called on CPUs with `avx512f` and `vpclmulqdq` support.
        unsafe {
            // Broadcast the scalar term to all lanes. We only need it in the lower 64 bits of each 128-bit lane.
            let t = i64::from_ne_bytes(term.to_ne_bytes());
            let term_vec = _mm512_set_epi64(0, t, 0, t, 0, t, 0, t);

            // Process chunks of 8
            let chunk_limit = len.saturating_sub(7);
            while i < chunk_limit {
                // Load 8 coefficients from source (unaligned)
                let s_ptr = source.as_ptr().add(i);
                // We need to unpack 8 contiguous 64-bit values into two 512-bit registers,
                // placing each 64-bit value into the lower half of a 128-bit lane.
                // Since they are contiguous in memory, we can't just do a single 512-bit load.
                // We could load them scalar, or use shuffle/unpack instructions.
                // For simplicity and to ensure correctness, we manually set them.
                // (In a heavily optimized pass, we could use AVX-512 gather or shuffle).
                let s0 = _mm512_set_epi64(
                    0,
                    i64::from_ne_bytes((*s_ptr.add(3)).to_ne_bytes()),
                    0,
                    i64::from_ne_bytes((*s_ptr.add(2)).to_ne_bytes()),
                    0,
                    i64::from_ne_bytes((*s_ptr.add(1)).to_ne_bytes()),
                    0,
                    i64::from_ne_bytes((*s_ptr.add(0)).to_ne_bytes()),
                );
                let s1 = _mm512_set_epi64(
                    0,
                    i64::from_ne_bytes((*s_ptr.add(7)).to_ne_bytes()),
                    0,
                    i64::from_ne_bytes((*s_ptr.add(6)).to_ne_bytes()),
                    0,
                    i64::from_ne_bytes((*s_ptr.add(5)).to_ne_bytes()),
                    0,
                    i64::from_ne_bytes((*s_ptr.add(4)).to_ne_bytes()),
                );

                // Multiply
                let p0 = gf64_mul_x4_avx512(term_vec, s0);
                let p1 = gf64_mul_x4_avx512(term_vec, s1);

                let mut tmp0 = AlignedArray([0u64; 8]);
                let mut tmp1 = AlignedArray([0u64; 8]);
                let tmp0_ptr = core::ptr::addr_of_mut!(tmp0).cast::<__m512i>();
                let tmp1_ptr = core::ptr::addr_of_mut!(tmp1).cast::<__m512i>();

                _mm512_store_si512(tmp0_ptr, p0);
                _mm512_store_si512(tmp1_ptr, p1);

                let t_ptr = target.as_mut_ptr().add(i);
                *t_ptr.add(0) ^= tmp0.0[0];
                *t_ptr.add(1) ^= tmp0.0[2];
                *t_ptr.add(2) ^= tmp0.0[4];
                *t_ptr.add(3) ^= tmp0.0[6];

                *t_ptr.add(4) ^= tmp1.0[0];
                *t_ptr.add(5) ^= tmp1.0[2];
                *t_ptr.add(6) ^= tmp1.0[4];
                *t_ptr.add(7) ^= tmp1.0[6];

                i = i.checked_add(8).expect("i cannot overflow len");
            }
        }

        // Handle remainder
        for idx in i..len {
            target[idx] ^= crate::reconcile::gf64::mul(term, source[idx]);
        }
    }
}

#[cfg(all(target_arch = "x86_64", has_avx512_support))]
#[target_feature(enable = "avx512f,avx512bw,vpclmulqdq")]
#[allow(clippy::incompatible_msrv)]
#[cfg_attr(coverage_nightly, coverage(off))]
// SAFETY: Only called by `Avx512Evaluator::poly_mac` which enforces CPU feature constraints.
unsafe fn gf64_mul_x4_avx512(a: __m512i, b: __m512i) -> __m512i {
    let product = _mm512_clmulepi64_epi128(a, b, 0x00);
    let high = _mm512_bsrli_epi128::<8>(product);

    let mut reduced = _mm512_xor_si512(product, high);
    reduced = _mm512_xor_si512(reduced, _mm512_slli_epi64::<1>(high));
    reduced = _mm512_xor_si512(reduced, _mm512_slli_epi64::<3>(high));
    reduced = _mm512_xor_si512(reduced, _mm512_slli_epi64::<4>(high));

    let low_mask = _mm512_set_epi64(0, -1, 0, -1, 0, -1, 0, -1);
    _mm512_and_si512(reduced, low_mask)
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
pub struct SseEvaluator;

#[cfg(target_arch = "x86_64")]
impl Gf64Evaluator for SseEvaluator {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn poly_mac(term: u64, source: &[u64], target: &mut [u64]) {
        // We could manually unroll PCLMULQDQ here, but for now we fallback to standard multiply.
        // The standard `mul` function is already hardware accelerated with PCLMULQDQ.
        for (i, &coefficient) in source.iter().enumerate() {
            target[i] ^= crate::reconcile::gf64::mul(term, coefficient);
        }
    }
}

#[derive(Clone, Copy)]
pub enum EvaluatorBackend {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Sse,
    #[cfg(all(target_arch = "x86_64", has_avx512_support))]
    Avx512,
}

pub fn get_evaluator() -> EvaluatorBackend {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static BACKEND: OnceLock<EvaluatorBackend> = OnceLock::new();
        *BACKEND.get_or_init(get_evaluator_internal)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        EvaluatorBackend::Scalar
    }
}

#[cfg(target_arch = "x86_64")]
fn get_evaluator_internal() -> EvaluatorBackend {
    #[cfg(has_avx512_support)]
    {
        if std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("vpclmulqdq")
        {
            return EvaluatorBackend::Avx512;
        }
    }
    if std::is_x86_feature_detected!("pclmulqdq") {
        EvaluatorBackend::Sse
    } else {
        EvaluatorBackend::Scalar
    }
}
