#![allow(unsafe_code)]

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

/// Evaluates polynomial roots in parallel blocks of 8.
pub trait Gf64Evaluator {
    /// Multiplies 8 Galois field elements by 8 roots simultaneously.
    fn eval_roots_x8(poly: &[u64], roots: &[u64; 8]) -> [u64; 8];
}

#[derive(Clone, Copy)]
pub struct ScalarEvaluator;

impl Gf64Evaluator for ScalarEvaluator {
    fn eval_roots_x8(poly: &[u64], roots: &[u64; 8]) -> [u64; 8] {
        let mut results = [0u64; 8];
        for i in 0..8 {
            let mut eval = 0u64;
            for &c in poly.iter().rev() {
                eval = crate::reconcile::gf64::mul(eval, roots[i]) ^ c;
            }
            results[i] = eval;
        }
        results
    }
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
pub struct Avx512Evaluator;

#[cfg(target_arch = "x86_64")]
impl Gf64Evaluator for Avx512Evaluator {
    fn eval_roots_x8(poly: &[u64], roots: &[u64; 8]) -> [u64; 8] {
        unsafe {
            // Load the 8 roots into two 512-bit registers (4 roots each, in the lower 64 bits of 128-bit lanes)
            let r0 = _mm512_set_epi64(
                0, roots[3] as i64,
                0, roots[2] as i64,
                0, roots[1] as i64,
                0, roots[0] as i64,
            );
            let r1 = _mm512_set_epi64(
                0, roots[7] as i64,
                0, roots[6] as i64,
                0, roots[5] as i64,
                0, roots[4] as i64,
            );

            let c_highest = poly[poly.len() - 1] as i64;
            let mut eval0 = _mm512_set_epi64(0, c_highest, 0, c_highest, 0, c_highest, 0, c_highest);
            let mut eval1 = _mm512_set_epi64(0, c_highest, 0, c_highest, 0, c_highest, 0, c_highest);

            for &c in poly.iter().rev().skip(1) {
                eval0 = gf64_mul_x4_avx512(eval0, r0);
                eval1 = gf64_mul_x4_avx512(eval1, r1);
                
                let c_vec = _mm512_set_epi64(0, c as i64, 0, c as i64, 0, c as i64, 0, c as i64);
                eval0 = _mm512_xor_si512(eval0, c_vec);
                eval1 = _mm512_xor_si512(eval1, c_vec);
            }

            let mut results = [0u64; 8];
            let mut tmp = [0u64; 8];
            _mm512_storeu_si512(tmp.as_mut_ptr() as *mut __m512i, eval0);
            results[0] = tmp[0];
            results[1] = tmp[2];
            results[2] = tmp[4];
            results[3] = tmp[6];
            
            _mm512_storeu_si512(tmp.as_mut_ptr() as *mut __m512i, eval1);
            results[4] = tmp[0];
            results[5] = tmp[2];
            results[6] = tmp[4];
            results[7] = tmp[6];

            results
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,vpclmulqdq")]
unsafe fn gf64_mul_x4_avx512(a: __m512i, b: __m512i) -> __m512i {
    let product = _mm512_clmulepi64_epi128(a, b, 0x00);
    let high = _mm512_bsrli_epi128::<8>(product);
    
    let mut reduced = _mm512_xor_si512(product, high);
    reduced = _mm512_xor_si512(reduced, _mm512_slli_epi64::<1>(high));
    reduced = _mm512_xor_si512(reduced, _mm512_slli_epi64::<3>(high));
    reduced = _mm512_xor_si512(reduced, _mm512_slli_epi64::<4>(high));
    
    let low_mask = _mm512_set_epi64(
        0, -1,
        0, -1,
        0, -1,
        0, -1,
    );
    _mm512_and_si512(reduced, low_mask)
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
pub struct SseEvaluator;

#[cfg(target_arch = "x86_64")]
impl Gf64Evaluator for SseEvaluator {
    fn eval_roots_x8(poly: &[u64], roots: &[u64; 8]) -> [u64; 8] {
        let mut results = [0u64; 8];
        for i in 0..8 {
            let mut eval = 0u64;
            for &c in poly.iter().rev() {
                eval = crate::reconcile::gf64::mul(eval, roots[i]) ^ c;
            }
            results[i] = eval;
        }
        results
    }
}

#[derive(Clone, Copy)]
pub enum EvaluatorBackend {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Sse,
    #[cfg(target_arch = "x86_64")]
    Avx512,
}

pub fn get_evaluator() -> EvaluatorBackend {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static BACKEND: OnceLock<EvaluatorBackend> = OnceLock::new();
        *BACKEND.get_or_init(|| {
            if std::is_x86_feature_detected!("avx512f") 
                && std::is_x86_feature_detected!("avx512bw")
                && std::is_x86_feature_detected!("vpclmulqdq") 
            {
                EvaluatorBackend::Avx512
            } else if std::is_x86_feature_detected!("pclmulqdq") {
                EvaluatorBackend::Sse
            } else {
                EvaluatorBackend::Scalar
            }
        })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        EvaluatorBackend::Scalar
    }
}
