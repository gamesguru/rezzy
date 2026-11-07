// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Phase 0 difference estimation and bucket localization for MSC4521.

use alloc::vec::Vec;

use super::client::MAX_BUCKETS_PER_ROUND;
use super::{
    pinsketch, AlgebraicError, Gf256, Gf256Decoder, Gf256Sketch, SyndromeSketch, MAX_DEPTH,
    STRATA_COUNT, STRATUM_CAPACITY,
};

/// Maximum sum of capacities in one bucketed sketch request.
pub const MAX_BUCKETED_SKETCH_CAPACITY: usize = 4_096;
/// Maximum extraction capacity assigned to one bucket.
pub const MAX_BUCKET_SKETCH_CAPACITY: usize = 32;
/// Recommended default work budget for [`decode_bucket_sketches`], sized to
/// cover a full normal-path batch: [`MAX_BUCKETS_PER_ROUND`] buckets each at
/// the worst-case per-bucket work ceiling for [`MAX_BUCKET_SKETCH_CAPACITY`].
/// A caller passing less than this to `decode_bucket_sketches` may cause a
/// bucket to be reported as failed because its fair share was insufficient;
/// see that function's doc comment.
pub const MAX_BATCH_FACTOR_WORK: usize = MAX_BUCKETS_PER_ROUND
    * match pinsketch::single_call_work_ceiling(MAX_BUCKET_SKETCH_CAPACITY) {
        Some(ceiling) => ceiling,
        None => panic!("single_call_work_ceiling overflowed for MAX_BUCKET_SKETCH_CAPACITY"),
    };

/// Returns a shared budget that grants every request its full per-bucket work
/// ceiling, including overflow requests.
///
/// This is useful when a batch contains capacities above the normal
/// [`MAX_BUCKET_SKETCH_CAPACITY`], which are not covered by
/// [`MAX_BATCH_FACTOR_WORK`]. A bucket can still be reported as failed if its
/// recursive factoring exceeds that ceiling.
///
/// # Errors
/// Returns an error if requests are invalid or summing their work ceilings
/// overflows `usize`.
pub fn recommended_bucket_decode_budget(
    requests: &[BucketRequest],
) -> Result<usize, AlgebraicError> {
    validate_adaptive_bucket_requests(requests)?;
    requests.iter().try_fold(0_usize, |total, request| {
        let ceiling = pinsketch::single_call_work_ceiling(request.capacity)
            .ok_or(AlgebraicError::InvalidSketchCapacity)?;
        total
            .checked_add(ceiling)
            .ok_or(AlgebraicError::InvalidSketchCapacity)
    })
}

/// Returns a shared budget for [`decode_gf256_bucket_sketches`] that grants
/// every request the decoder's full per-sketch GF(2^256) allowance.
///
/// This is the GF(2^256) counterpart of [`recommended_bucket_decode_budget`].
/// Using the GF(2^64) helper for a GF(2^256) batch under-budgets it and can
/// cause ordinary buckets to be reported as failed. The
/// GF(2^256) decoder clamps each bucket to
/// `Gf256Decoder::per_sketch_work_ceiling`, so the shared budget must grant
/// that full allowance for every request rather than a per-capacity estimate.
///
/// # Errors
/// Returns an error if requests are invalid or the total allowance overflows
/// `usize`.
pub fn recommended_gf256_bucket_decode_budget(
    requests: &[BucketRequest],
) -> Result<usize, AlgebraicError> {
    validate_adaptive_bucket_requests(requests)?;
    requests
        .len()
        .checked_mul(Gf256Decoder::per_sketch_work_ceiling())
        .ok_or(AlgebraicError::InvalidSketchCapacity)
}

struct PayloadCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> PayloadCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, capacity: usize, width: usize) -> Result<&'a [u8], AlgebraicError> {
        let byte_len = capacity
            .checked_mul(width)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        let end = self
            .offset
            .checked_add(byte_len)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        self.offset = end;
        Ok(bytes)
    }

    const fn is_finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn charge_shared_budget(
    budget: &mut usize,
    allowance: usize,
    remaining: usize,
) -> Result<(), AlgebraicError> {
    // The decoder should only subtract from its initial allowance. Check that
    // invariant in debug builds, but retain checked arithmetic in release:
    // these paths process untrusted sketches, and a panic is worse than a
    // decode-accounting error.
    debug_assert!(
        remaining <= allowance,
        "decoder violated budget bounds: remaining={remaining} > allowance={allowance}"
    );
    let spent = allowance
        .checked_sub(remaining)
        .ok_or(AlgebraicError::BudgetExhausted)?;
    *budget = budget
        .checked_sub(spent)
        .ok_or(AlgebraicError::BudgetExhausted)?;
    Ok(())
}

fn shared_bucket_allowance(
    budget: usize,
    ceiling: usize,
    remaining_requests: usize,
    remaining_ceilings: Option<usize>,
) -> usize {
    debug_assert!(remaining_requests > 0);
    if remaining_ceilings.is_some_and(|total| budget >= total) {
        ceiling
    } else {
        budget
            .checked_div(remaining_requests)
            .unwrap_or(0)
            .min(ceiling)
    }
}
/// Recommended default work budget for [`estimate_strata`], sized to
/// cover a full strata-estimator pass: [`STRATA_COUNT`] strata each at
/// the worst-case per-stratum work ceiling for [`STRATUM_CAPACITY`].
/// Without this, each stratum gets its own implicit `MAX_FACTOR_WORK`
/// from the unbudgeted `pinsketch::decode` path, so a batch of 32
/// strata could cost up to 32× that ceiling -- the strata count was
/// not a cost input. See `estimate_strata`'s doc comment.
pub const MAX_STRATA_FACTOR_WORK: usize = STRATA_COUNT
    * match pinsketch::single_call_work_ceiling(STRATUM_CAPACITY) {
        Some(ceiling) => ceiling,
        None => panic!("single_call_work_ceiling overflowed for STRATUM_CAPACITY"),
    };
/// Maximum capacity permitted only through the local overflow request path.
///
/// This is not a negotiated protocol capability. Normal bucket requests remain
/// limited to [`MAX_BUCKET_SKETCH_CAPACITY`].
// Kept equal to `algebraic::MAX_OVERFLOW_SKETCH_CAPACITY` — see its comment.
pub const MAX_OVERFLOW_BUCKET_CAPACITY: usize = 256;
/// Client-side sketch-mode cutoff for estimates in the saturated regime.
pub const SATURATED_DELTA_ESTIMATE: u64 = 8 * (1_u64 << 31);
/// Minimum cardinality implied by an over-capacity stratum-0 decode failure.
const OVER_CAPACITY_DELTA_FLOOR: u64 = (STRATUM_CAPACITY as u64) + 1;

/// One localized sketch request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketRequest {
    /// Prefix depth of the bucket, in bits.
    pub depth: u8,
    /// The bucket's prefix.
    pub prefix: u64,
    /// Sketch capacity requested for the bucket.
    pub capacity: usize,
    /// Explicit overflow marker. Must be set to `true` only after the overflow
    /// request validation path passes. Never infer from `capacity` alone.
    pub overflow: bool,
}

impl BucketRequest {
    /// Standard bucket request (overflow = false).
    #[must_use]
    pub const fn new(depth: u8, prefix: u64, capacity: usize) -> Self {
        Self {
            depth,
            prefix,
            capacity,
            overflow: false,
        }
    }

    /// Overflow bucket request (overflow = true).
    #[must_use]
    pub const fn with_overflow(depth: u8, prefix: u64, capacity: usize) -> Self {
        Self {
            depth,
            prefix,
            capacity,
            overflow: true,
        }
    }
}

/// Serialized length of a [`NodeSummary`]: a 16-byte digest then an 8-byte
/// count, both big-endian (the digest matches the level-0 accumulator wire
/// order).
pub const NODE_SUMMARY_LEN: usize = 24;

/// Per-node accumulator carried with each bucket sketch: the count of the
/// node's population and the XOR of `h128` over it.
///
/// This is the node-scoped counterpart of the level-0 `(digest, count)` pair,
/// and is what lets each node be verified, split or discarded on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeSummary {
    /// Number of elements in the node.
    pub count: u64,
    /// XOR of `h128` over the node's elements.
    pub digest: u128,
}

impl NodeSummary {
    /// Whether two child summaries reconstruct this parent: counts add
    /// (overflow is inconsistency) and digests XOR.
    #[must_use]
    pub fn is_split_of(self, left: Self, right: Self) -> bool {
        left.count.checked_add(right.count) == Some(self.count)
            && self.digest == left.digest ^ right.digest
    }

    /// Summarizes the `h128` values of one node's elements.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::CountOverflow`] if the count exceeds `u64`.
    pub fn from_h128s(h128s: &[u128]) -> Result<Self, AlgebraicError> {
        Ok(Self {
            count: u64::try_from(h128s.len()).map_err(|_| AlgebraicError::CountOverflow)?,
            digest: h128s.iter().fold(0, |acc, h| acc ^ h),
        })
    }

    /// Serializes as digest (16 bytes) then count (8 bytes), big-endian.
    #[must_use]
    pub fn to_bytes(self) -> [u8; NODE_SUMMARY_LEN] {
        let mut out = [0_u8; NODE_SUMMARY_LEN];
        out[..16].copy_from_slice(&self.digest.to_be_bytes());
        out[16..].copy_from_slice(&self.count.to_be_bytes());
        out
    }

    /// Parses the [`to_bytes`](Self::to_bytes) encoding.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::InvalidSketchLength`] unless `bytes` is
    /// exactly [`NODE_SUMMARY_LEN`] long.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AlgebraicError> {
        let bytes: &[u8; NODE_SUMMARY_LEN] = bytes
            .try_into()
            .map_err(|_| AlgebraicError::InvalidSketchLength)?;
        let (digest, count) = bytes.split_at(16);
        Ok(Self {
            digest: u128::from_be_bytes(
                digest
                    .try_into()
                    .map_err(|_| AlgebraicError::InvalidSketchLength)?,
            ),
            count: u64::from_be_bytes(
                count
                    .try_into()
                    .map_err(|_| AlgebraicError::InvalidSketchLength)?,
            ),
        })
    }
}

/// Roots recovered from one independently decoded bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketDecodeSuccess {
    /// Prefix depth of the bucket, in bits.
    pub depth: u8,
    /// The bucket's prefix.
    pub prefix: u64,
    /// Short identifiers recovered from the bucket.
    pub roots: Vec<u64>,
}

/// Partial result of decoding a concatenated bucket sketch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketDecodeBatch {
    /// Buckets that decoded.
    pub successful_buckets: Vec<BucketDecodeSuccess>,
    /// Each entry is `(depth, prefix)` — the full bucket identifier, not prefix alone.
    pub failed_buckets: Vec<(u8, u64)>,
}

impl BucketDecodeBatch {
    /// A batch in which every requested bucket failed to decode, as when the
    /// peer reports capacity exceeded for the whole request.
    #[must_use]
    pub fn all_failed(requests: &[BucketRequest]) -> Self {
        Self {
            successful_buckets: Vec::new(),
            failed_buckets: requests.iter().map(|r| (r.depth, r.prefix)).collect(),
        }
    }

    /// A batch in which every requested bucket decoded, with `roots` grouped
    /// into the bucket whose prefix they fall under. Buckets holding no root
    /// still succeed with an empty root list. Roots outside every requested
    /// bucket are dropped; requests are expected to be non-overlapping.
    #[must_use]
    pub fn from_roots(requests: &[BucketRequest], roots: &[u64]) -> Self {
        let successful_buckets = requests
            .iter()
            .map(|r| BucketDecodeSuccess {
                depth: r.depth,
                prefix: r.prefix,
                roots: roots
                    .iter()
                    .copied()
                    .filter(|&root| bucket_prefix(root, r.depth) == r.prefix)
                    .collect(),
            })
            .collect();
        Self {
            successful_buckets,
            failed_buckets: Vec::new(),
        }
    }
}

/// Returns the bucket prefix of `h64` at `depth`: its top `depth` bits.
///
/// Depth 0 is the single root bucket (prefix 0); depths of 64 or more return
/// `h64` unchanged.
#[must_use]
pub const fn bucket_prefix(h64: u64, depth: u8) -> u64 {
    if depth == 0 {
        0
    } else if depth >= 64 {
        h64
    } else {
        h64 >> 64_u32.saturating_sub(depth as u32)
    }
}

/// Returns the canonical start of a bucket's key-space range.
///
/// This is shared between bucket ordering and request validation so the two
/// paths stay aligned if the bucket geometry changes.
#[must_use]
pub(crate) fn bucket_range_start(request: &BucketRequest) -> u128 {
    let shift = u32::from(MAX_DEPTH.saturating_sub(request.depth));
    u128::from(request.prefix) << shift
}

/// Estimated symmetric-difference cardinality derived from the strata sketches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrataEstimate {
    /// Estimated symmetric-difference cardinality.
    pub delta: u64,
    /// Whether the estimate is provisional because decoding stopped at an
    /// over-capacity stratum and had to extrapolate from the decoded tail.
    pub low_confidence: bool,
}

/// Estimates the symmetric difference from corresponding strata sketches.
///
/// This helper stays test-only so production callers use the structured
/// [`StrataEstimate`] API rather than a bare scalar estimate.
///
/// # Errors
/// Returns an error when root finding exceeds its work budget.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn estimate_delta(
    local: &[[u64; STRATUM_CAPACITY]; STRATA_COUNT],
    remote: &[[u64; STRATUM_CAPACITY]; STRATA_COUNT],
) -> Result<u64, AlgebraicError> {
    Ok(estimate_delta_internal(local, remote, MAX_STRATA_FACTOR_WORK)?.0)
}

/// Estimates the symmetric difference and whether that estimate is provisional.
///
/// Starting at the sparsest stratum, this decodes the longest consecutive tail.
/// If `r` is the lowest decoded stratum and `T` is the decoded tail cardinality,
/// `T * 2^r` estimates the complete difference. Decoding every stratum yields
/// the exact cardinality.
///
/// If even the sparsest residual stratum overflows, this returns a saturated
/// estimate in [`StrataEstimate::delta`] and marks it
/// [`StrataEstimate::low_confidence`] so the caller can route away from sketch
/// mode.
///
/// `budget` bounds the *total* factoring work across all 32 strata, not just
/// each stratum individually. Without this, each stratum gets its own implicit
/// `MAX_FACTOR_WORK` from the unbudgeted decode path, so a full pass could
/// cost up to 32× that ceiling -- the strata count was not a cost input.
/// [`MAX_STRATA_FACTOR_WORK`] is sized to cover a full pass and is the right
/// default absent a caller-specific budget.
///
/// # Errors
/// Returns an error when root finding exceeds its work budget.
pub fn estimate_strata(
    local: &[[u64; STRATUM_CAPACITY]; STRATA_COUNT],
    remote: &[[u64; STRATUM_CAPACITY]; STRATA_COUNT],
    budget: usize,
) -> Result<StrataEstimate, AlgebraicError> {
    let (delta, low_confidence) = estimate_delta_internal(local, remote, budget)?;
    Ok(StrataEstimate {
        delta,
        low_confidence,
    })
}

fn estimate_delta_internal(
    local: &[[u64; STRATUM_CAPACITY]; STRATA_COUNT],
    remote: &[[u64; STRATUM_CAPACITY]; STRATA_COUNT],
    mut budget: usize,
) -> Result<(u64, bool), AlgebraicError> {
    let mut decoded_tail = 0_u64;
    let mut lowest_decoded = None;

    for stratum in (0..STRATA_COUNT).rev() {
        if local[stratum] == remote[stratum] {
            // A zero residual is already its own empty decode; skip factoring.
            lowest_decoded = Some(stratum);
            continue;
        }
        let residual: [u64; STRATUM_CAPACITY] =
            core::array::from_fn(|index| local[stratum][index] ^ remote[stratum][index]);

        let ceiling = pinsketch::single_call_work_ceiling(STRATUM_CAPACITY)
            .ok_or(AlgebraicError::InvalidSketchCapacity)?;
        let allowance = budget.min(ceiling);
        let mut remaining = allowance;
        match pinsketch::decode_with_budget(&residual, STRATUM_CAPACITY, &mut remaining) {
            Ok(roots) => {
                let cardinality =
                    u64::try_from(roots.len()).map_err(|_| AlgebraicError::CountOverflow)?;
                decoded_tail = decoded_tail
                    .checked_add(cardinality)
                    .ok_or(AlgebraicError::CountOverflow)?;
                lowest_decoded = Some(stratum);
            }

            Err(AlgebraicError::DecodeFailure) => {
                if lowest_decoded.is_none() && stratum == STRATA_COUNT - 1 {
                    return Ok((SATURATED_DELTA_ESTIMATE, true));
                }

                let scaled_stratum = lowest_decoded.unwrap_or(stratum);
                let shift =
                    u32::try_from(scaled_stratum).map_err(|_| AlgebraicError::CountOverflow)?;
                let estimate = decoded_tail
                    .max(OVER_CAPACITY_DELTA_FLOOR)
                    .saturating_mul(1_u64 << shift);

                return Ok((estimate, true));
            }
            Err(error) => return Err(error),
        }
        debug_assert!(
            remaining <= allowance,
            "estimator decode violated budget bounds: remaining={remaining} > allowance={allowance}"
        );
        let spent = allowance
            .checked_sub(remaining)
            .ok_or(AlgebraicError::BudgetExhausted)?;
        budget = budget
            .checked_sub(spent)
            .ok_or(AlgebraicError::BudgetExhausted)?;
    }

    let stratum = lowest_decoded.expect("all strata decoded implies stratum 0 decoded");
    let shift = u32::try_from(stratum).map_err(|_| AlgebraicError::CountOverflow)?;
    // saturating_mul overflows to u64::MAX rather than silently collapsing to 0
    // (which the old checked_shl(shift).unwrap_or(0) scale factor could do).
    Ok((decoded_tail.saturating_mul(1_u64 << shift), false))
}

/// Decodes a batch of bucket sketches under a single, shared work budget.
///
/// Requests are validated in canonical key-space range order. Each requested
/// sketch is serialized as little-endian syndrome coordinates with no length
/// prefix; the request capacities define the boundaries. Structural errors
/// abort the batch. Decode and bucket-budget failures are isolated to their
/// buckets so successfully decoded roots can be retained.
///
/// `budget` bounds the *total* factoring work across every bucket in the
/// batch, not just each bucket individually. Without this, each bucket gets
/// its own implicit `MAX_FACTOR_WORK` from the unbudgeted decode path, so a
/// batch of many buckets (up to `MAX_BUCKETED_SKETCH_CAPACITY` worth of
/// aggregate capacity) could cost the caller an unbounded multiple of a
/// single bucket's work -- the batch size itself was not a cost input.
///
/// Each bucket's draw is clamped to its per-bucket ceiling. If the shared
/// budget cannot fund every remaining request up to its ceiling, it is divided
/// evenly among the remaining requests; unused work rolls forward. A bucket
/// that exhausts its share is reported as failed, and later buckets still get
/// a share instead of being starved by request order.
/// [`MAX_BATCH_FACTOR_WORK`] is sized to cover
/// a full normal-path batch and is the right default absent a
/// caller-specific budget.
///
/// # Errors
///
/// Returns an error for invalid ordering, capacity, aggregate or byte
/// length, or an arithmetic error charging the shared budget. A bucket that
/// exhausts its allowance is instead reported in `failed_buckets`, allowing
/// the caller to retry or split that node. [`recommended_bucket_decode_budget`]
/// grants each request its full ceiling in one shared budget.
pub fn decode_bucket_sketches(
    encoded: &[u8],
    requests: &[BucketRequest],
    mut budget: usize,
) -> Result<BucketDecodeBatch, AlgebraicError> {
    validate_adaptive_bucket_requests(requests)?;

    let ceilings: Vec<usize> = requests
        .iter()
        .map(|request| {
            pinsketch::single_call_work_ceiling(request.capacity)
                .ok_or(AlgebraicError::InvalidSketchCapacity)
        })
        .collect::<Result<_, _>>()?;
    let mut payload = PayloadCursor::new(encoded);
    let mut successful_buckets = Vec::new();
    let mut failed_buckets = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        let bytes = payload.take(request.capacity, 8)?;

        let coordinates = bytes
            .chunks_exact(8)
            .map(|coordinate| {
                let mut value = [0; 8];
                value.copy_from_slice(coordinate);
                u64::from_le_bytes(value)
            })
            .collect();

        let sketch = if request.overflow {
            SyndromeSketch::from_overflow_coordinates(coordinates)?
        } else {
            SyndromeSketch::from_coordinates(coordinates)?
        };
        // Clamp this bucket's draw so it can't outspend its own fair
        // ceiling and starve buckets later in `requests` of the shared
        // pool -- see this function's doc comment.
        let ceiling = ceilings[index];
        let remaining_requests = requests.len().saturating_sub(index);
        let remaining_ceilings = ceilings[index..]
            .iter()
            .try_fold(0_usize, |total, ceiling| total.checked_add(*ceiling));
        let allowance =
            shared_bucket_allowance(budget, ceiling, remaining_requests, remaining_ceilings);
        let mut remaining = allowance;
        let result = if request.overflow {
            sketch.decode_elements_overflow_with_shared_budget(request.capacity, &mut remaining)
        } else {
            sketch.decode_elements_with_shared_budget(request.capacity, &mut remaining)
        };
        charge_shared_budget(&mut budget, allowance, remaining)?;
        match result {
            Ok(roots) => successful_buckets.push(BucketDecodeSuccess {
                depth: request.depth,
                prefix: request.prefix,
                roots,
            }),
            // A starved shared pool is a failed bucket too, so earlier
            // successes survive and the caller can retry or split.
            Err(AlgebraicError::DecodeFailure | AlgebraicError::BudgetExhausted) => {
                failed_buckets.push((request.depth, request.prefix));
            }
            Err(error) => return Err(error),
        }
    }
    if !payload.is_finished() {
        return Err(AlgebraicError::InvalidSketchLength);
    }
    Ok(BucketDecodeBatch {
        successful_buckets,
        failed_buckets,
    })
}

/// One successfully decoded GF(2^256) bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gf256BucketDecodeSuccess {
    /// Prefix depth of the bucket, in bits.
    pub depth: u8,
    /// The bucket's prefix.
    pub prefix: u64,
    /// Full 32-byte digests of the symmetric difference, sorted by field
    /// element order.
    pub digests: Vec<[u8; 32]>,
}

/// Result of [`decode_gf256_bucket_sketches`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Gf256BucketDecodeBatch {
    /// Buckets that decoded.
    pub successful_buckets: Vec<Gf256BucketDecodeSuccess>,
    /// Each entry is `(depth, prefix)`.
    pub failed_buckets: Vec<(u8, u64)>,
}

/// Parses and decodes concatenated GF(2^256) residual bucket sketches under
/// one shared work budget.
///
/// Each request occupies `32 * capacity` bytes, in canonical range order, with
/// no length prefix. Each bucket is allowed at most the per-sketch GF(2^256)
/// ceiling. If the shared budget cannot cover every remaining bucket ceiling,
/// the remainder is divided evenly across the buckets still requiring decode;
/// unused work rolls forward. This prevents an early expensive bucket from
/// taking the entire pool.
///
/// A bucket that fails to decode, or that exhausts its full per-sketch
/// ceiling or the remaining shared pool, is reported in `failed_buckets`;
/// earlier successes are kept so the caller can retry or split it with a
/// fresh allowance. Structural errors abort.
///
/// `budget` has no default: the GF(2^256) batch budget has not been
/// re-derived (see `MAX_BATCH_FACTOR_WORK`, which is GF(2^64)-sized), so the
/// caller must choose one explicitly.
///
/// # Errors
/// Returns an error for invalid request ordering or capacity, a payload whose
/// length is not exactly the sum of the request sizes, or a non-canonical
/// field element. Exhausted bucket allowances are recorded in
/// `failed_buckets`.
pub fn decode_gf256_bucket_sketches(
    encoded: &[u8],
    requests: &[BucketRequest],
    budget: usize,
    decoder: &Gf256Decoder,
) -> Result<Gf256BucketDecodeBatch, AlgebraicError> {
    decode_gf256_bucket_sketches_inner(encoded, requests, budget, decoder, None)
}

/// Decodes a GF(2^256) batch while skipping buckets whose local and remote
/// summaries are both empty and whose corresponding sketch bytes are all zero.
/// Skipped buckets produce an empty success outcome. Nonzero payload bytes are
/// decoded even with empty summaries so the batch verifier can reject an
/// inconsistent responder. The strata estimate does not participate in this
/// decision.
///
/// # Errors
/// Returns the same errors as [`decode_gf256_bucket_sketches`], and
/// [`AlgebraicError::InvalidSketchLength`] if either summary slice is not
/// aligned with `requests`.
pub fn decode_gf256_bucket_sketches_with_summaries(
    encoded: &[u8],
    requests: &[BucketRequest],
    local: &[NodeSummary],
    remote: &[NodeSummary],
    budget: usize,
    decoder: &Gf256Decoder,
) -> Result<Gf256BucketDecodeBatch, AlgebraicError> {
    if local.len() != requests.len() || remote.len() != requests.len() {
        return Err(AlgebraicError::InvalidSketchLength);
    }
    decode_gf256_bucket_sketches_inner(encoded, requests, budget, decoder, Some((local, remote)))
}

fn decode_gf256_bucket_sketches_inner(
    encoded: &[u8],
    requests: &[BucketRequest],
    mut budget: usize,
    decoder: &Gf256Decoder,
    summaries: Option<(&[NodeSummary], &[NodeSummary])>,
) -> Result<Gf256BucketDecodeBatch, AlgebraicError> {
    validate_adaptive_bucket_requests(requests)?;

    let expected_len = requests.iter().try_fold(0_usize, |total, request| {
        let byte_len = request.capacity.checked_mul(32)?;
        total.checked_add(byte_len)
    });
    if expected_len != Some(encoded.len()) {
        return Err(AlgebraicError::InvalidSketchLength);
    }

    let ceiling = Gf256Decoder::per_sketch_work_ceiling();
    let mut active_requests = 0_usize;
    let mut offset = 0_usize;
    for (index, request) in requests.iter().enumerate() {
        let length = request
            .capacity
            .checked_mul(32)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        let end = offset
            .checked_add(length)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        let bytes = encoded
            .get(offset..end)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        if !should_skip_empty_gf256_bucket(bytes, index, summaries) {
            active_requests = active_requests.saturating_add(1);
        }
        offset = end;
    }
    let mut payload = PayloadCursor::new(encoded);
    let mut batch = Gf256BucketDecodeBatch::default();
    for (index, request) in requests.iter().enumerate() {
        let bytes = payload.take(request.capacity, 32)?;

        if should_skip_empty_gf256_bucket(bytes, index, summaries) {
            batch.successful_buckets.push(Gf256BucketDecodeSuccess {
                depth: request.depth,
                prefix: request.prefix,
                digests: Vec::new(),
            });
            continue;
        }

        let sketch = Gf256Sketch::from_bytes(request.capacity, bytes)?;
        let remaining_ceilings = active_requests.checked_mul(ceiling);
        let allowance =
            shared_bucket_allowance(budget, ceiling, active_requests, remaining_ceilings);
        active_requests = active_requests.saturating_sub(1);
        let mut remaining = allowance;
        let result = sketch.decode_elements_with_budget(request.capacity, decoder, &mut remaining);
        charge_shared_budget(&mut budget, allowance, remaining)?;
        match result {
            Ok(elements) => batch.successful_buckets.push(Gf256BucketDecodeSuccess {
                depth: request.depth,
                prefix: request.prefix,
                digests: elements.into_iter().map(Gf256::to_le_bytes).collect(),
            }),
            Err(AlgebraicError::DecodeFailure | AlgebraicError::BudgetExhausted) => {
                batch.failed_buckets.push((request.depth, request.prefix));
            }
            Err(error) => return Err(error),
        }
    }
    debug_assert!(payload.is_finished());
    Ok(batch)
}

fn should_skip_empty_gf256_bucket(
    bytes: &[u8],
    index: usize,
    summaries: Option<(&[NodeSummary], &[NodeSummary])>,
) -> bool {
    summaries.is_some_and(|(local, remote)| {
        local[index] == NodeSummary::default()
            && remote[index] == NodeSummary::default()
            && bytes.iter().all(|byte| *byte == 0)
    })
}

/// Validates a list of bucket requests to ensure they adhere to limits and form an antichain.
///
/// Normatively, the request set MUST be an antichain under the prefix-containment relation.
/// For two requests `R_i = (d_i, p_i)` and `R_j = (d_j, p_j)`, `R_i` is an ancestor of `R_j`
/// if and only if `d_i <= d_j` and the `d_i` most-significant bits of `p_j` equal `p_i`.
/// A receiver MUST reject any request list containing an ancestor/descendant pair before
/// performing sketch subtraction or field operations.
///
/// This function also ensures no single request exceeds the per-node extraction limit
/// (`MAX_BUCKET_SKETCH_CAPACITY`), that the overall extraction respects
/// `MAX_BUCKETED_SKETCH_CAPACITY`, and that bucket indices are well-formed.
///
/// Implementation note (non-normative): a canonical `O(N log N)` verifier can sort requests
/// by ascending `depth`, then compare each candidate only against previously validated shallower
/// requests using the same prefix test. A binary prefix trie can reduce this to `O(N)`.
///
/// # Errors
/// Returns an error if any capacity or bound constraint is violated, or if the requests
/// overlap.
pub fn validate_bucket_requests(requests: &[BucketRequest]) -> Result<(), AlgebraicError> {
    for request in requests {
        if request.overflow {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
    }
    validate_bucket_requests_with_limit(requests, MAX_BUCKET_SKETCH_CAPACITY)
}

/// Validates requests issued through the explicit local overflow path.
///
/// Normal request handling must continue to call [`validate_bucket_requests`].
/// This helper does not negotiate or advertise overflow support to peers.
///
/// # Errors
/// Returns an error when a request exceeds the overflow or aggregate capacity
/// limit, or when requests are malformed or overlap.
pub fn validate_overflow_bucket_requests(requests: &[BucketRequest]) -> Result<(), AlgebraicError> {
    for request in requests {
        if !request.overflow {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
    }
    validate_bucket_requests_with_limit(requests, MAX_OVERFLOW_BUCKET_CAPACITY)
}

/// Validates one adaptive round containing ordinary and explicit overflow
/// requests, while retaining the shared aggregate and overlap limits.
///
/// # Errors
/// Returns an error if a request has invalid capacity, bounds, or overlap.
pub fn validate_adaptive_bucket_requests(requests: &[BucketRequest]) -> Result<(), AlgebraicError> {
    validate_bucket_requests_with_limit(requests, MAX_OVERFLOW_BUCKET_CAPACITY)
}

fn validate_bucket_requests_with_limit(
    requests: &[BucketRequest],
    max_bucket_capacity: usize,
) -> Result<(), AlgebraicError> {
    let mut total_capacity = 0_usize;
    let mut previous_end = 0_u128;
    for request in requests {
        let item_limit = if request.overflow {
            max_bucket_capacity
        } else {
            MAX_BUCKET_SKETCH_CAPACITY
        };
        if request.capacity == 0 || request.capacity > item_limit {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
        if request.depth > MAX_DEPTH {
            return Err(AlgebraicError::InvalidBucketIndex);
        }

        if request.prefix >= (1_u64 << request.depth) {
            return Err(AlgebraicError::InvalidBucketIndex);
        }

        total_capacity = total_capacity
            .checked_add(request.capacity)
            .ok_or(AlgebraicError::InvalidSketchCapacity)?;
        if total_capacity > MAX_BUCKETED_SKETCH_CAPACITY {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }

        let start = bucket_range_start(request);
        let shift = u32::from(MAX_DEPTH.saturating_sub(request.depth));
        let end = start
            .checked_add(1_u128 << shift)
            .ok_or(AlgebraicError::InvalidBucketIndex)?;

        if start < previous_end {
            return Err(AlgebraicError::InvalidBucketIndex);
        }
        previous_end = end;
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::{ElementHash, ResidentKernel};

    #[test]
    fn test_validate_bucket_requests_rejects_overlap() {
        // Correct disjoint requests
        assert!(validate_bucket_requests(&[BucketRequest::new(0, 0, 4)]).is_ok());

        // Nested ranges: depth 0 prefix 0 contains depth 1 prefix 0
        assert!(validate_bucket_requests(&[
            BucketRequest::new(0, 0, 4),
            BucketRequest::new(1, 0, 4)
        ])
        .is_err());

        // Same-depth out-of-order ranges are rejected.
        assert!(validate_bucket_requests(&[
            BucketRequest::new(1, 1, 4),
            BucketRequest::new(1, 0, 4)
        ])
        .is_err());

        // Same-depth disjoint ranges in canonical order are valid.
        assert!(validate_bucket_requests(&[
            BucketRequest::new(1, 0, 4),
            BucketRequest::new(1, 1, 4)
        ])
        .is_ok());

        // Nested ranges remain invalid in any order.
        assert!(validate_bucket_requests(&[
            BucketRequest::new(1, 0, 4),
            BucketRequest::new(0, 0, 4),
        ])
        .is_err());
    }

    #[test]
    fn test_validate_bucket_requests_enforces_depth_31_prefix_bounds() {
        assert!(validate_bucket_requests(&[BucketRequest::new(31, (1_u64 << 31) - 1, 4)]).is_ok());

        assert_eq!(
            validate_bucket_requests(&[BucketRequest::new(31, 1_u64 << 31, 4)]),
            Err(AlgebraicError::InvalidBucketIndex)
        );
    }

    #[test]
    fn test_validate_bucket_requests_accepts_full_h64_depth() {
        assert!(validate_bucket_requests(&[BucketRequest::new(
            MAX_DEPTH,
            (1_u64 << MAX_DEPTH) - 1,
            4
        )])
        .is_ok());
        // One bit past the cap, and one level past it, are both rejected.
        assert_eq!(
            validate_bucket_requests(&[BucketRequest::new(MAX_DEPTH, 1_u64 << MAX_DEPTH, 4)]),
            Err(AlgebraicError::InvalidBucketIndex)
        );
        assert_eq!(
            validate_bucket_requests(&[BucketRequest::new(MAX_DEPTH + 1, 0, 4)]),
            Err(AlgebraicError::InvalidBucketIndex)
        );
    }

    #[test]
    fn overflow_requests_allow_larger_sketches_but_normal_requests_do_not() {
        let request = BucketRequest::with_overflow(1, 0, MAX_OVERFLOW_BUCKET_CAPACITY);

        assert_eq!(
            validate_bucket_requests(&[request]),
            Err(AlgebraicError::InvalidSketchCapacity)
        );
        assert!(validate_overflow_bucket_requests(&[request]).is_ok());
    }

    #[test]
    fn overflow_requests_enforce_the_aggregate_capacity_limit() {
        // depth 6 gives 64 distinct, non-overlapping prefixes -- enough
        // headroom to push the aggregate (43 * MAX_OVERFLOW_BUCKET_CAPACITY)
        // past MAX_BUCKETED_SKETCH_CAPACITY without any single request
        // exceeding the per-request cap.
        let requests = (0..43)
            .map(|prefix| BucketRequest::with_overflow(6, prefix, MAX_OVERFLOW_BUCKET_CAPACITY))
            .collect::<Vec<_>>();

        assert_eq!(
            validate_overflow_bucket_requests(&requests),
            Err(AlgebraicError::InvalidSketchCapacity)
        );
    }

    fn toggle_stratum(strata: &mut [[u64; STRATUM_CAPACITY]; STRATA_COUNT], value: u64) {
        let event = ElementHash {
            h128: u128::from(value),
            h64: value,
            q3: value,
        };
        let mut resident = ResidentKernel::new();
        resident.insert(event).unwrap();
        for (target, source) in strata.iter_mut().zip(resident.strata()) {
            for (coordinate, value) in target.iter_mut().zip(source) {
                *coordinate ^= value;
            }
        }
    }

    fn populate_stratum(
        strata: &mut [[u64; STRATUM_CAPACITY]; STRATA_COUNT],
        stratum: usize,
        odd_values: &[u64],
    ) {
        for odd in odd_values {
            toggle_stratum(strata, odd << stratum);
        }
    }

    #[test]
    fn strata_tail_is_exact_when_every_stratum_decodes() {
        let local = [[0; STRATUM_CAPACITY]; STRATA_COUNT];
        let mut remote = local;
        for value in [1, 2, 4, 8, 3, 5] {
            toggle_stratum(&mut remote, value);
        }
        assert_eq!(estimate_delta(&local, &remote), Ok(6));
        assert_eq!(
            estimate_strata(&local, &remote, MAX_STRATA_FACTOR_WORK),
            Ok(StrataEstimate {
                delta: 6,
                low_confidence: false,
            })
        );
        assert_eq!(estimate_delta(&local, &local), Ok(0));
    }

    #[test]
    fn empty_sparse_tail_uses_low_confidence_tail_estimate() {
        let local = [[0; STRATUM_CAPACITY]; STRATA_COUNT];
        let mut remote = local;
        for value in (1..=17).step_by(2) {
            toggle_stratum(&mut remote, value);
        }
        assert_eq!(estimate_delta(&local, &remote), Ok(18));
    }

    #[test]
    fn strata_estimator_marks_stratum_zero_overflow_low_confidence() {
        let local = [[0; STRATUM_CAPACITY]; STRATA_COUNT];
        let mut remote = local;
        for value in (1..=17).step_by(2) {
            toggle_stratum(&mut remote, value);
        }

        assert_eq!(
            pinsketch::decode(&remote[0], STRATUM_CAPACITY),
            Err(AlgebraicError::DecodeFailure)
        );
        assert_eq!(
            estimate_strata(&local, &remote, MAX_STRATA_FACTOR_WORK),
            Ok(StrataEstimate {
                delta: 18,
                low_confidence: true,
            })
        );
    }

    #[test]
    fn low_confidence_estimate_uses_lowest_decoded_stratum() {
        let local = [[0; STRATUM_CAPACITY]; STRATA_COUNT];
        let mut remote = local;

        populate_stratum(&mut remote, 7, &[1, 3, 5, 7, 9]);
        populate_stratum(&mut remote, 6, &[1, 3, 5, 7, 9]);
        populate_stratum(&mut remote, 5, &[1, 3, 5, 7, 9]);
        populate_stratum(&mut remote, 4, &[1, 3, 5, 7, 9]);
        populate_stratum(&mut remote, 3, &[1, 3, 5, 7, 9, 11, 13, 15, 17]);

        assert_eq!(estimate_delta(&local, &remote), Ok(320));
        assert_eq!(
            estimate_strata(&local, &remote, MAX_STRATA_FACTOR_WORK),
            Ok(StrataEstimate {
                delta: 320,
                low_confidence: true,
            })
        );
    }

    #[test]
    fn highest_stratum_overflow_remains_unmeasurable() {
        let local = [[0; STRATUM_CAPACITY]; STRATA_COUNT];
        let mut remote = local;
        populate_stratum(
            &mut remote,
            STRATA_COUNT - 1,
            &[1, 3, 5, 7, 9, 11, 13, 15, 17],
        );

        assert_eq!(
            pinsketch::decode(&remote[STRATA_COUNT - 1], STRATUM_CAPACITY),
            Err(AlgebraicError::DecodeFailure)
        );
        assert_eq!(
            estimate_delta(&local, &remote),
            Ok(SATURATED_DELTA_ESTIMATE)
        );
        assert_eq!(
            estimate_strata(&local, &remote, MAX_STRATA_FACTOR_WORK),
            Ok(StrataEstimate {
                delta: SATURATED_DELTA_ESTIMATE,
                low_confidence: true,
            })
        );
    }

    #[test]
    fn bucket_prefix_and_batch_constructors() {
        assert_eq!(bucket_prefix(u64::MAX, 0), 0);
        assert_eq!(bucket_prefix(1 << 63, 1), 1);
        assert_eq!(bucket_prefix(0xABCD_0000_0000_0000, 16), 0xABCD);
        assert_eq!(bucket_prefix(7, 64), 7);
        let requests = [BucketRequest::new(1, 0, 4), BucketRequest::new(2, 2, 4)];
        let failed = BucketDecodeBatch::all_failed(&requests);
        assert_eq!(failed.failed_buckets, [(1, 0), (2, 2)]);
        assert!(failed.successful_buckets.is_empty());
        let (a, b) = (0x1000_0000_0000_0000_u64, 0x8000_0000_0000_0001_u64);
        let batch = BucketDecodeBatch::from_roots(&requests, &[a, b]);
        assert!(batch.failed_buckets.is_empty());
        assert_eq!(batch.successful_buckets[0].roots, [a]);
        assert_eq!(batch.successful_buckets[1].roots, [b]);
        assert_eq!(batch.successful_buckets[1].prefix, 2);
    }

    #[test]
    fn bucket_decoder_retains_successes_and_isolates_decode_failures() {
        let requests = [BucketRequest::new(8, 1, 2), BucketRequest::new(8, 9, 2)];
        let mut first = SyndromeSketch::new(2).unwrap();
        first.toggle(7).unwrap();
        let mut encoded = first
            .coordinates()
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        let mut over_capacity = SyndromeSketch::new(2).unwrap();
        for value in [1, 2, 3] {
            over_capacity.toggle(value).unwrap();
        }
        encoded.extend(
            over_capacity
                .coordinates()
                .iter()
                .flat_map(|value| value.to_le_bytes()),
        );

        assert_eq!(
            decode_bucket_sketches(&encoded, &requests, MAX_BATCH_FACTOR_WORK),
            Ok(BucketDecodeBatch {
                successful_buckets: vec![BucketDecodeSuccess {
                    depth: 8,
                    prefix: 1,
                    roots: vec![7],
                }],
                failed_buckets: vec![(8, 9)],
            })
        );
    }

    fn encoded_sketch(
        capacity: usize,
        overflow: bool,
        values: impl IntoIterator<Item = u64>,
    ) -> Vec<u8> {
        let mut sketch = if overflow {
            SyndromeSketch::new_overflow(capacity).unwrap()
        } else {
            SyndromeSketch::new(capacity).unwrap()
        };
        for value in values {
            sketch.toggle(value).unwrap();
        }
        sketch
            .coordinates()
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn syndrome_work_used(capacity: usize, roots: impl IntoIterator<Item = u64>) -> usize {
        let mut sketch = SyndromeSketch::new(capacity).unwrap();
        for root in roots {
            sketch.toggle(root).unwrap();
        }
        let ceiling = pinsketch::single_call_work_ceiling(capacity).unwrap();
        let mut remaining = ceiling;
        sketch
            .decode_elements_with_shared_budget(capacity, &mut remaining)
            .unwrap();
        ceiling
            .checked_sub(remaining)
            .expect("decode remaining work cannot exceed its allowance")
    }

    fn fair_share_requests() -> [BucketRequest; 2] {
        [BucketRequest::new(1, 0, 8), BucketRequest::new(1, 1, 8)]
    }

    fn assert_first_prefix_failed(
        failed_buckets: &[(u8, u64)],
        successful_buckets: usize,
        first_prefix: u64,
    ) {
        assert_eq!(failed_buckets, [(1, 0)]);
        assert_eq!(successful_buckets, 1);
        assert_eq!(first_prefix, 1);
    }

    #[test]
    fn bucket_decoder_isolates_per_bucket_budget_exhaustion() {
        let requests = [BucketRequest::new(8, 1, 32), BucketRequest::new(8, 9, 4)];
        let mut encoded = encoded_sketch(32, false, 1..=33);
        encoded.extend(encoded_sketch(4, false, [7]));

        let budget = pinsketch::single_call_work_ceiling(32)
            .unwrap()
            .checked_add(pinsketch::single_call_work_ceiling(4).unwrap())
            .unwrap();
        let batch = decode_bucket_sketches(&encoded, &requests, budget).unwrap();
        assert_eq!(batch.failed_buckets, [(8, 1)]);
        assert_eq!(batch.successful_buckets.len(), 1);
        assert_eq!(batch.successful_buckets[0].prefix, 9);
        assert_eq!(batch.successful_buckets[0].roots, [7]);
    }

    #[test]
    fn bucket_decoder_reports_failed_bucket_when_shared_budget_is_exhausted() {
        let requests = [BucketRequest::new(8, 1, 4)];
        let encoded = encoded_sketch(4, false, [7, 11, 19]);

        let batch = decode_bucket_sketches(&encoded, &requests, 0).unwrap();
        assert_eq!(batch.failed_buckets, [(8, 1)]);
        assert_eq!(batch.successful_buckets.len(), 0);
        // Retry with a fresh full allowance succeeds.
        let fresh = recommended_bucket_decode_budget(&requests).unwrap();
        let retry = decode_bucket_sketches(&encoded, &requests, fresh).unwrap();
        assert_eq!(retry.failed_buckets.len(), 0);
        assert_eq!(retry.successful_buckets[0].roots, [7, 11, 19]);
    }

    #[test]
    fn bucket_decoder_fair_shares_sub_ceiling_budget_with_later_bucket() {
        let expensive_roots = 1..=8;
        let cheap_roots = [100];
        let expensive_work = syndrome_work_used(8, expensive_roots.clone());
        let cheap_work = syndrome_work_used(8, cheap_roots);
        assert!(expensive_work > cheap_work);

        let mut encoded = encoded_sketch(8, false, expensive_roots);
        encoded.extend(encoded_sketch(8, false, cheap_roots));
        let requests = fair_share_requests();
        let batch = decode_bucket_sketches(&encoded, &requests, 2 * cheap_work).unwrap();

        assert_first_prefix_failed(
            &batch.failed_buckets,
            batch.successful_buckets.len(),
            batch.successful_buckets[0].prefix,
        );
        assert_eq!(batch.successful_buckets[0].roots, cheap_roots);
    }

    fn assert_overflow_round_trip(request: BucketRequest, count: usize, budget: usize) {
        let expected: Vec<u64> = (1..=count as u64)
            .map(|value| value.saturating_mul(2).saturating_add(1))
            .collect();
        let encoded = encoded_sketch(request.capacity, true, expected.iter().copied());
        let batch = decode_bucket_sketches(&encoded, &[request], budget).unwrap();
        assert!(batch.failed_buckets.is_empty(), "{batch:?}");
        let mut roots = batch.successful_buckets[0].roots.clone();
        roots.sort_unstable();
        assert_eq!(roots, expected);
    }

    #[test]
    fn overflow_bucket_above_standard_capacity_round_trips_over_the_wire() {
        assert_overflow_round_trip(
            BucketRequest::with_overflow(8, 1, 64),
            40,
            MAX_BATCH_FACTOR_WORK,
        );
    }

    #[test]
    fn capacity_256_overflow_request_decodes_with_its_single_call_ceiling() {
        let budget = pinsketch::single_call_work_ceiling(MAX_OVERFLOW_BUCKET_CAPACITY).unwrap();
        assert_overflow_round_trip(
            BucketRequest::with_overflow(0, 0, MAX_OVERFLOW_BUCKET_CAPACITY),
            MAX_OVERFLOW_BUCKET_CAPACITY,
            budget,
        );
    }

    #[test]
    fn one_undecodable_bucket_does_not_starve_a_later_decodable_bucket() {
        // Bucket at prefix 1 is over capacity and forces the full
        // factoring ladder to run and fail. Bucket at prefix 9 is
        // trivially decodable. The shared budget is exactly one
        // capacity-2 ceiling plus a small margin.
        //
        // For an unclamped root-only failure (no further splitting -- the
        // only case a small deterministic test can construct without
        // engineering a specific locator factorization by hand) the spend
        // is bounded by `single_call_work_ceiling` regardless of whether
        // this clamp exists, so this test cannot by itself distinguish
        // clamped from unclamped code; it pins the intended budget-sharing
        // behavior (the easy bucket must not starve) as a regression guard
        // going forward. The clamp's real justification is a bucket whose
        // recursion visits several nodes before failing, where the total
        // cost can exceed one node's ceiling -- see `MAX_FACTOR_WORK`'s
        // comment on unbalanced split trees; that scenario needs a
        // deliberately engineered locator to reproduce deterministically
        // and isn't covered by this test.
        let requests = [BucketRequest::new(8, 1, 2), BucketRequest::new(8, 9, 2)];
        let mut encoded = encoded_sketch(2, false, [1, 2, 3]);
        encoded.extend(encoded_sketch(2, false, [7]));

        let ceiling = pinsketch::single_call_work_ceiling(2).unwrap();
        let budget = ceiling.saturating_add(1_000);

        let batch = decode_bucket_sketches(&encoded, &requests, budget).unwrap();
        assert!(
            batch
                .successful_buckets
                .iter()
                .any(|success| success.prefix == 9 && success.roots == vec![7]),
            "the decodable bucket at prefix 9 must not be starved by the \
             undecodable bucket at prefix 1 running first: {batch:?}"
        );
    }

    #[test]
    fn bucket_decoder_rejects_length_mismatches_and_nested_overlaps() {
        let unordered = [BucketRequest::new(8, 2, 1), BucketRequest::new(8, 1, 1)];
        assert_eq!(
            decode_bucket_sketches(&[0; 16], &unordered, MAX_BATCH_FACTOR_WORK),
            Err(AlgebraicError::InvalidBucketIndex)
        );

        let nested = [BucketRequest::new(0, 0, 1), BucketRequest::new(1, 0, 1)];
        assert_eq!(
            decode_bucket_sketches(&[0; 16], &nested, MAX_BATCH_FACTOR_WORK),
            Err(AlgebraicError::InvalidBucketIndex)
        );
        assert_eq!(
            decode_bucket_sketches(
                &[0; 7],
                &[BucketRequest::new(8, 1, 1)],
                MAX_BATCH_FACTOR_WORK
            ),
            Err(AlgebraicError::InvalidSketchLength)
        );
        assert_eq!(
            decode_bucket_sketches(
                &[0; 9],
                &[BucketRequest::new(8, 1, 1)],
                MAX_BATCH_FACTOR_WORK
            ),
            Err(AlgebraicError::InvalidSketchLength)
        );
    }

    #[test]
    fn strata_estimator_exhausts_budget_cleanly() {
        let local = [[0; STRATUM_CAPACITY]; STRATA_COUNT];
        let mut remote = local;
        populate_stratum(&mut remote, 0, &[1, 3, 5]);

        // A tiny budget must cleanly fail with BudgetExhausted without
        // panic or miscalculation.
        assert_eq!(
            estimate_strata(&local, &remote, 10),
            Err(AlgebraicError::BudgetExhausted)
        );
    }

    use crate::test_util::digest32 as gf256_digest;

    fn gf256_payload(capacity: usize, seeds: &[u64]) -> (Vec<u8>, Vec<[u8; 32]>) {
        let mut sketch = Gf256Sketch::new(capacity).unwrap();
        let mut digests: Vec<[u8; 32]> = seeds.iter().map(|s| gf256_digest(*s)).collect();
        for digest in &digests {
            sketch
                .toggle(Gf256::from_le_bytes(digest).unwrap())
                .unwrap();
        }
        digests.sort_by_key(|d| Gf256::from_le_bytes(d).unwrap());
        (sketch.to_bytes(), digests)
    }

    // The budget starts at the compile-time ceiling and every production
    // charge uses checked_sub, returning BudgetExhausted rather than going
    // negative. This expect therefore exposes a broken decoder invariant, not
    // malformed input. It is test-only; production decoding keeps its normal
    // error path.
    fn gf256_work_used(decoder: &Gf256Decoder, capacity: usize, seeds: &[u64]) -> usize {
        let (encoded, _) = gf256_payload(capacity, seeds);
        let sketch = Gf256Sketch::from_bytes(capacity, &encoded).unwrap();
        let ceiling = Gf256Decoder::per_sketch_work_ceiling();
        let mut remaining = ceiling;
        sketch
            .decode_elements_with_budget(capacity, decoder, &mut remaining)
            .unwrap();
        ceiling
            .checked_sub(remaining)
            .expect("GF256 decode cannot consume more work than its ceiling")
    }

    fn decode_gf256_with_summary(
        encoded: &[u8],
        summary: NodeSummary,
        budget: usize,
        decoder: &Gf256Decoder,
    ) -> Result<Gf256BucketDecodeBatch, AlgebraicError> {
        let request = [BucketRequest::new(0, 0, 4)];
        let summaries = [summary];
        decode_gf256_bucket_sketches_with_summaries(
            encoded, &request, &summaries, &summaries, budget, decoder,
        )
    }

    #[test]
    fn gf256_batch_recovers_full_digests_per_bucket() {
        let decoder = Gf256Decoder::build().unwrap();
        let (mut encoded, left) = gf256_payload(8, &[1, 2, 3]);
        let (second, right) = gf256_payload(4, &[10, 11]);
        encoded.extend_from_slice(&second);
        let requests = [BucketRequest::new(1, 0, 8), BucketRequest::new(1, 1, 4)];
        let batch =
            decode_gf256_bucket_sketches(&encoded, &requests, 100_000_000, &decoder).unwrap();
        assert!(batch.failed_buckets.is_empty());
        assert_eq!(batch.successful_buckets.len(), 2);
        assert_eq!(batch.successful_buckets[0].digests, left);
        assert_eq!(batch.successful_buckets[1].digests, right);
        assert_eq!(
            (
                batch.successful_buckets[1].depth,
                batch.successful_buckets[1].prefix
            ),
            (1, 1)
        );
    }

    #[test]
    fn gf256_batch_isolates_an_over_capacity_bucket() {
        let decoder = Gf256Decoder::build().unwrap();
        let seeds: Vec<u64> = (100..140).collect();
        let (mut encoded, _) = gf256_payload(4, &seeds);
        let (second, right) = gf256_payload(4, &[7]);
        encoded.extend_from_slice(&second);
        let requests = [BucketRequest::new(1, 0, 4), BucketRequest::new(1, 1, 4)];
        let full_allowance = recommended_gf256_bucket_decode_budget(&requests).unwrap();
        let batch =
            decode_gf256_bucket_sketches(&encoded, &requests, full_allowance, &decoder).unwrap();
        assert_eq!(batch.failed_buckets, vec![(1, 0)]);
        assert_eq!(batch.successful_buckets.len(), 1);
        assert_eq!(batch.successful_buckets[0].digests, right);
    }

    #[test]
    fn gf256_batch_empty_sketch_decodes_to_no_digests() {
        let decoder = Gf256Decoder::build().unwrap();
        let encoded = Gf256Sketch::new(4).unwrap().to_bytes();
        let batch =
            decode_gf256_bucket_sketches(&encoded, &[BucketRequest::new(0, 0, 4)], 0, &decoder)
                .unwrap();
        assert_eq!(batch.successful_buckets.len(), 1);
        assert!(batch.successful_buckets[0].digests.is_empty());
    }

    #[test]
    fn gf256_batch_skips_decode_when_node_summaries_match() {
        let decoder = Gf256Decoder::build().unwrap();
        let encoded = Gf256Sketch::new(4).unwrap().to_bytes();
        let batch =
            decode_gf256_with_summary(&encoded, NodeSummary::default(), 0, &decoder).unwrap();
        assert!(batch.failed_buckets.is_empty());
        assert_eq!(batch.successful_buckets.len(), 1);
        assert!(batch.successful_buckets[0].digests.is_empty());
    }

    #[test]
    fn gf256_batch_decodes_nonzero_payload_with_empty_summaries() {
        let decoder = Gf256Decoder::build().unwrap();
        let request = [BucketRequest::new(0, 0, 4)];
        let summaries = [NodeSummary::default()];
        let (encoded, expected) = gf256_payload(4, &[1]);
        let batch = decode_gf256_bucket_sketches_with_summaries(
            &encoded,
            &request,
            &summaries,
            &summaries,
            100_000_000,
            &decoder,
        )
        .unwrap();
        assert_eq!(batch.successful_buckets[0].digests, expected);

        let (verified_batch, verified) =
            crate::verify_gf256_batch(batch, &request, &summaries, &summaries, |_| false).unwrap();
        assert!(verified.is_empty());
        assert!(verified_batch.successful_buckets.is_empty());
        assert_eq!(verified_batch.failed_buckets, vec![(0, 0)]);
    }

    #[test]
    fn gf256_batch_does_not_skip_equal_nonempty_summaries() {
        let decoder = Gf256Decoder::build().unwrap();
        let summary = NodeSummary {
            count: 10,
            digest: 0x1234,
        };
        let (encoded, _) = gf256_payload(4, &[1]);

        let batch = decode_gf256_with_summary(&encoded, summary, 0, &decoder).unwrap();
        assert!(batch.failed_buckets.is_empty());
        assert_eq!(batch.successful_buckets.len(), 1);
        assert_eq!(batch.successful_buckets[0].digests.len(), 1);
    }

    #[test]
    fn gf256_batch_shared_budget_exhaustion_is_a_failed_bucket() {
        let decoder = Gf256Decoder::build().unwrap();
        let (encoded, expected) = gf256_payload(8, &[1, 2, 3, 4, 5, 6]);
        let request = [BucketRequest::new(0, 0, 8)];
        let batch = decode_gf256_bucket_sketches(&encoded, &request, 0, &decoder).unwrap();
        assert_eq!(batch.failed_buckets, [(0, 0)]);
        assert_eq!(batch.successful_buckets.len(), 0);
        // Retry with a fresh full allowance succeeds.
        let fresh = recommended_gf256_bucket_decode_budget(&request).unwrap();
        let retry = decode_gf256_bucket_sketches(&encoded, &request, fresh, &decoder).unwrap();
        assert_eq!(retry.failed_buckets.len(), 0);
        assert_eq!(retry.successful_buckets[0].digests.len(), expected.len());
    }

    #[test]
    fn gf256_batch_rejects_bad_length_and_requests() {
        let decoder = Gf256Decoder::build().unwrap();
        let (encoded, _) = gf256_payload(4, &[1]);
        let one = [BucketRequest::new(0, 0, 4)];
        let budget = 100_000_000;
        assert_eq!(
            decode_gf256_bucket_sketches(&encoded[..encoded.len() - 1], &one, budget, &decoder),
            Err(AlgebraicError::InvalidSketchLength)
        );
        let mut long = encoded;
        long.push(0);
        assert_eq!(
            decode_gf256_bucket_sketches(&long, &one, budget, &decoder),
            Err(AlgebraicError::InvalidSketchLength)
        );
        let nested = [BucketRequest::new(0, 0, 4), BucketRequest::new(1, 0, 4)];
        assert!(decode_gf256_bucket_sketches(&[0; 256], &nested, budget, &decoder).is_err());
        let unordered = [BucketRequest::new(1, 1, 4), BucketRequest::new(1, 0, 4)];
        assert!(decode_gf256_bucket_sketches(&[0; 256], &unordered, budget, &decoder).is_err());
    }

    #[test]
    fn gf256_batch_empty_batch_and_oversized_capacity() {
        let decoder = Gf256Decoder::build().unwrap();
        let empty = decode_gf256_bucket_sketches(&[], &[], 0, &decoder).unwrap();
        assert!(empty.successful_buckets.is_empty() && empty.failed_buckets.is_empty());
        let big = BucketRequest::new(0, 0, MAX_BUCKET_SKETCH_CAPACITY + 1);
        assert_eq!(
            decode_gf256_bucket_sketches(&vec![0; 33 * 32], &[big], usize::MAX, &decoder),
            Err(AlgebraicError::InvalidSketchCapacity)
        );
    }

    #[test]
    fn gf256_batch_fair_shares_sub_ceiling_budget_with_later_bucket() {
        let decoder = Gf256Decoder::build().unwrap();
        let expensive_seeds: Vec<u64> = (1..=8).collect();
        let cheap_seeds = [100];
        let expensive_work = gf256_work_used(&decoder, 8, &expensive_seeds);
        let cheap_work = gf256_work_used(&decoder, 8, &cheap_seeds);
        assert!(expensive_work > cheap_work);

        let (mut encoded, _) = gf256_payload(8, &expensive_seeds);
        let (second, expected) = gf256_payload(8, &cheap_seeds);
        encoded.extend_from_slice(&second);
        let requests = fair_share_requests();
        let batch =
            decode_gf256_bucket_sketches(&encoded, &requests, 2 * cheap_work, &decoder).unwrap();

        assert_first_prefix_failed(
            &batch.failed_buckets,
            batch.successful_buckets.len(),
            batch.successful_buckets[0].prefix,
        );
        assert_eq!(batch.successful_buckets[0].digests, expected);
    }

    #[test]
    fn recommended_gf256_budget_grants_full_per_sketch_allowance() {
        let requests = [BucketRequest::new(1, 0, 32), BucketRequest::new(1, 1, 32)];
        assert_eq!(
            recommended_gf256_bucket_decode_budget(&requests).unwrap(),
            2 * Gf256Decoder::per_sketch_work_ceiling()
        );
    }

    #[cfg(feature = "std")]
    fn calibration_next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[cfg(feature = "std")]
    fn calibration_roots(
        repetition: usize,
        bucket: usize,
        count: usize,
        random: bool,
    ) -> Vec<Gf256> {
        let rep = u64::try_from(repetition).unwrap();
        let bucket = u64::try_from(bucket).unwrap();
        let mut state = 0x6a09_e667_f3bc_c909_u64
            ^ rep.wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ bucket.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        let mut roots = Vec::with_capacity(count);
        while roots.len() < count {
            let candidate = if random {
                Gf256::from_limbs(core::array::from_fn(|_| calibration_next(&mut state)))
            } else {
                Gf256::from_limbs([
                    u64::try_from(roots.len()).unwrap().wrapping_add(1),
                    bucket.wrapping_add(1),
                    rep.wrapping_add(1),
                    0,
                ])
            };
            if candidate != Gf256::ZERO && !roots.contains(&candidate) {
                roots.push(candidate);
            }
        }
        roots.sort_unstable();
        roots
    }

    #[cfg(feature = "std")]
    fn calibration_stats(values: &[u64]) -> Option<(u64, u64, u64, u64)> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let nearest_rank = |percentile: usize| {
            let rank = u64::try_from(percentile)
                .expect("percentile fits u64")
                .checked_mul(u64::try_from(sorted.len()).expect("sample count fits u64"))
                .expect("calibration percentile multiplication fits u64");
            sorted[usize::try_from(rank.div_ceil(100).saturating_sub(1))
                .expect("percentile rank fits usize")]
        };
        Some((
            nearest_rank(50),
            nearest_rank(95),
            nearest_rank(99),
            *sorted.last().unwrap(),
        ))
    }

    #[cfg(feature = "std")]
    fn print_calibration_stats(
        pattern: &str,
        scenario: &str,
        scope: &str,
        outcome: &str,
        values: &[u64],
    ) {
        if let Some((p50, p95, p99, max)) = calibration_stats(values) {
            std::println!(
                "{pattern},{scenario},{scope},{outcome},{},{p50},{p95},{p99},{max}",
                values.len()
            );
        }
    }

    #[cfg(feature = "std")]
    #[test]
    #[ignore = "release GF256 batch work-unit calibration"]
    fn gf256_128_bucket_batch_work_calibration() {
        const REPETITIONS: usize = 1_000;
        const BUCKETS: usize = MAX_BUCKETS_PER_ROUND;
        const CAPACITY: usize = MAX_BUCKET_SKETCH_CAPACITY;
        let decoder = Gf256Decoder::build().expect("cached GF256 decoder");
        let per_bucket_allowance = Gf256Decoder::per_sketch_work_ceiling();
        // The theoretical batch total is tracked as u64 so it remains
        // representable on 32-bit targets; only each per-bucket allowance is
        // converted back to usize for the decoder.
        let batch_allowance = u64::try_from(per_bucket_allowance)
            .expect("per-bucket allowance fits u64")
            .checked_mul(u64::try_from(BUCKETS).expect("bucket count fits u64"))
            .expect("batch allowance fits u64");

        std::println!("pattern,scenario,scope,outcome,samples,p50,p95,p99,max");
        for (pattern, random) in [("random", true), ("consecutive", false)] {
            for (scenario, total_differences, stress_bucket) in [
                ("total_10", 10_usize, false),
                ("total_100", 100, false),
                ("total_1000", 1_000, false),
                ("total_10000", 10_000, false),
                ("worst_bucket_32", 0, true),
            ] {
                let mut bucket_success = Vec::with_capacity(REPETITIONS * BUCKETS);
                let mut bucket_failure = Vec::new();
                let mut bucket_all = Vec::with_capacity(REPETITIONS * BUCKETS);
                let mut batch_success = Vec::with_capacity(REPETITIONS);
                let mut batch_failure = Vec::with_capacity(REPETITIONS);
                let mut batch_all = Vec::with_capacity(REPETITIONS);
                let mut total_successes = 0_usize;
                let mut total_failures = 0_usize;

                for repetition in 0..REPETITIONS {
                    let mut counts = vec![0_usize; BUCKETS];
                    if stress_bucket {
                        counts[repetition % BUCKETS] = CAPACITY;
                    } else {
                        let mut placement_state =
                            0x243f_6a88_85a3_08d3_u64 ^ u64::try_from(repetition).unwrap();
                        for _ in 0..total_differences {
                            let bucket = (calibration_next(&mut placement_state) >> (u64::BITS - 7))
                                as usize;
                            counts[bucket] += 1;
                        }
                    }

                    let mut remaining_batch = batch_allowance;
                    let mut this_batch_success = 0_u64;
                    let mut this_batch_failure = 0_u64;
                    for (bucket, count) in counts.into_iter().enumerate() {
                        let expected = calibration_roots(repetition, bucket, count, random);
                        let mut sketch = Gf256Sketch::new(CAPACITY).unwrap();
                        for root in &expected {
                            sketch.toggle(*root).unwrap();
                        }

                        let allowance = usize::try_from(
                            remaining_batch
                                .min(u64::try_from(per_bucket_allowance).expect("fits u64")),
                        )
                        .expect("batch allowance fits usize");
                        let mut remaining_bucket = allowance;
                        let decoded = decoder.decode_with_budget(
                            sketch.coordinates(),
                            CAPACITY,
                            &mut remaining_bucket,
                        );
                        let spent = u64::try_from(allowance - remaining_bucket)
                            .expect("spent work fits u64");
                        remaining_batch = remaining_batch.checked_sub(spent).unwrap();
                        bucket_all.push(spent);

                        let exact = decoded.is_ok_and(|mut actual| {
                            actual.sort_unstable();
                            actual == expected
                        });
                        if exact {
                            total_successes += 1;
                            this_batch_success += spent;
                            bucket_success.push(spent);
                        } else {
                            total_failures += 1;
                            this_batch_failure += spent;
                            bucket_failure.push(spent);
                        }
                    }
                    assert_eq!(
                        batch_allowance - remaining_batch,
                        this_batch_success + this_batch_failure
                    );
                    batch_success.push(this_batch_success);
                    batch_failure.push(this_batch_failure);
                    batch_all.push(this_batch_success + this_batch_failure);
                }

                std::println!("# {pattern},{scenario},bucket_counts,success={total_successes},failed={total_failures}");
                print_calibration_stats(pattern, scenario, "bucket", "success", &bucket_success);
                print_calibration_stats(pattern, scenario, "bucket", "failed", &bucket_failure);
                print_calibration_stats(pattern, scenario, "bucket", "all", &bucket_all);
                print_calibration_stats(pattern, scenario, "batch", "success_work", &batch_success);
                print_calibration_stats(pattern, scenario, "batch", "failed_work", &batch_failure);
                print_calibration_stats(pattern, scenario, "batch", "total_work", &batch_all);
            }
        }
    }
}
