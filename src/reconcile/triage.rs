// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Phase 0 difference estimation and bucket localization for MSC0500.

use alloc::vec::Vec;

use super::{
    AlgebraicError, BUCKET_COUNT, ResidentBucket, STRATA_COUNT, STRATUM_CAPACITY, SyndromeSketch,
    pinsketch,
};

/// Maximum sum of capacities in one bucketed sketch request.
pub const MAX_BUCKETED_SKETCH_CAPACITY: usize = 4_096;
/// Maximum extraction capacity assigned to one bucket.
pub const MAX_BUCKET_SKETCH_CAPACITY: usize = 64;

/// One responder bucket summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteBucketSummary {
    pub accumulator: u128,
    pub count: u32,
}

/// One differing bucket and its deterministic count lower bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketDifference {
    pub bucket_id: u8,
    pub count_delta: u32,
}

/// One localized sketch request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketRequest {
    pub depth: u8,
    pub prefix: u32,
    pub capacity: usize,
}

/// Roots recovered from one independently decoded bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketDecodeSuccess {
    pub depth: u8,
    pub prefix: u32,
    pub roots: Vec<u64>,
}

/// Partial result of decoding a concatenated bucket sketch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketDecodeBatch {
    pub successful_buckets: Vec<BucketDecodeSuccess>,
    pub failed_buckets: Vec<u32>,
}

/// Estimates the symmetric difference from corresponding strata sketches.
///
/// Starting at the sparsest stratum, this decodes the longest consecutive tail.
/// If `r` is the lowest decoded stratum and `T` is the decoded tail cardinality,
/// `T * 2^r` estimates the complete difference. Decoding every stratum yields
/// the exact cardinality.
///
/// `None` means even the sparsest stratum exceeded its resident capacity.
///
/// # Errors
/// Returns an error when root finding exceeds its work budget.
pub fn estimate_delta(
    local: &[[u64; STRATUM_CAPACITY]; STRATA_COUNT],
    remote: &[[u64; STRATUM_CAPACITY]; STRATA_COUNT],
) -> Result<Option<u64>, AlgebraicError> {
    let mut decoded_tail = 0_u64;
    let mut lowest_decoded = None;

    for stratum in (0..STRATA_COUNT).rev() {
        let residual: [u64; STRATUM_CAPACITY] =
            core::array::from_fn(|index| local[stratum][index] ^ remote[stratum][index]);
        match pinsketch::decode(&residual, STRATUM_CAPACITY) {
            Ok(roots) => {
                let cardinality =
                    u64::try_from(roots.len()).map_err(|_| AlgebraicError::CountOverflow)?;
                decoded_tail = decoded_tail
                    .checked_add(cardinality)
                    .ok_or(AlgebraicError::CountOverflow)?;
                lowest_decoded = Some(stratum);
            }
            Err(AlgebraicError::DecodeFailure) => break,
            Err(error) => return Err(error),
        }
    }

    let Some(stratum) = lowest_decoded else {
        return Ok(None);
    };
    if decoded_tail == 0 && stratum != 0 {
        return Ok(None);
    }
    let shift = u32::try_from(stratum).map_err(|_| AlgebraicError::CountOverflow)?;
    Ok(Some(decoded_tail.checked_shl(shift).unwrap_or(u64::MAX)))
}

/// Locates buckets whose accumulator or exact count differs.
///
/// # Errors
/// Returns [`AlgebraicError::InvalidSketchLength`] unless both summaries contain
/// exactly 256 buckets, or [`AlgebraicError::CountOverflow`] when a local count
/// is marked inexact and must be repaired before count-based provisioning.
pub fn select_differing_buckets(
    local: &[ResidentBucket],
    remote: &[RemoteBucketSummary],
) -> Result<Vec<BucketDifference>, AlgebraicError> {
    if local.len() != BUCKET_COUNT || remote.len() != BUCKET_COUNT {
        return Err(AlgebraicError::InvalidSketchLength);
    }
    if local.iter().any(|bucket| bucket.scan_required) {
        return Err(AlgebraicError::CountOverflow);
    }

    local
        .iter()
        .zip(remote)
        .enumerate()
        .filter(|(_, (local_bucket, remote_bucket))| {
            local_bucket.accumulator != remote_bucket.accumulator
                || local_bucket.count != remote_bucket.count
        })
        .map(|(index, (local_bucket, remote_bucket))| {
            u8::try_from(index)
                .map(|bucket_id| BucketDifference {
                    bucket_id,
                    count_delta: local_bucket.count.abs_diff(remote_bucket.count),
                })
                .map_err(|_| AlgebraicError::InvalidBucketIndex)
        })
        .collect()
}

/// Represents a provisioned batch of bucket requests, which may be truncated if the
/// aggregate budget is exceeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedBuckets {
    pub requests: Vec<BucketRequest>,
    pub truncated: bool,
}

/// Dispatches deep-triage bucket requests for MSC0501 bucket exchange.
///
/// Converts a list of locally detected depth-8 bucket differences into
/// target capacities and optionally deepens the prefix if the estimated
/// difference per bucket is too large.
///
/// # Errors
/// Returns an error for invalid sketch configurations or aggregate limits.
pub fn provision_bucket_capacities(
    differences: &[BucketDifference],
    estimated_delta: Option<u64>,
    aggregate_cap: usize,
) -> Result<ProvisionedBuckets, AlgebraicError> {
    if aggregate_cap == 0 || aggregate_cap > MAX_BUCKETED_SKETCH_CAPACITY {
        return Err(AlgebraicError::InvalidSketchCapacity);
    }

    let mut total = 0_usize;
    let mut requests = Vec::new();

    let differing_count = differences.len().max(1);
    let est = estimated_delta
        .and_then(|e| usize::try_from(e).ok())
        .unwrap_or_else(|| differences.iter().map(|d| d.count_delta as usize).sum());

    let lambda = 3_usize; // Safe bucket capacity target (k=8, lambda=3)
    let per_bucket_est = est.div_ceil(differing_count);

    let mut depth_shift = 0;
    if per_bucket_est > lambda {
        let required_depth = est.div_ceil(lambda).next_power_of_two().trailing_zeros();
        depth_shift = required_depth.saturating_sub(8);
    }

    // Cap the depth shift so we don't overflow prefix bits or generate absurd amounts of buckets
    depth_shift = depth_shift.min(16);
    let target_depth = 8_u8.saturating_add(u8::try_from(depth_shift).unwrap_or(0));

    for difference in differences {
        // We use lambda as the base capacity if we are splitting.
        // If not splitting (depth_shift == 0), we provision based on the bucket's own count_delta.
        let capacity = if depth_shift > 0 {
            8_usize // Deep splits use a fixed safe capacity of 8
        } else {
            let count_delta = usize::try_from(difference.count_delta)
                .map_err(|_| AlgebraicError::CountOverflow)?
                .max(per_bucket_est);
            count_delta
                .checked_add(count_delta / 2)
                .and_then(|c| c.checked_add(count_delta % 2))
                .and_then(|c| c.checked_add(4))
                .unwrap_or(usize::MAX)
                .clamp(STRATUM_CAPACITY, MAX_BUCKET_SKETCH_CAPACITY)
        };

        let sub_bucket_count = 1_u32 << depth_shift;
        for sub in 0..sub_bucket_count {
            if total.checked_add(capacity).is_none()
                || total.saturating_add(capacity) > aggregate_cap
            {
                return Err(AlgebraicError::InvalidSketchCapacity);
            }

            let prefix = (u32::from(difference.bucket_id) << depth_shift) | sub;
            requests.push(BucketRequest {
                depth: target_depth,
                prefix,
                capacity,
            });
            total = total.saturating_add(capacity);
        }
    }

    if let Some(estimate) = estimated_delta {
        if differences.is_empty() && estimate != 0 {
            return Err(AlgebraicError::DecodeFailure);
        }
        let estimate =
            usize::try_from(estimate).map_err(|_| AlgebraicError::InvalidSketchCapacity)?;
        let target = estimate
            .checked_add(estimate / 2)
            .and_then(|capacity| capacity.checked_add(estimate % 2))
            .and_then(|capacity| capacity.checked_add(4))
            .ok_or(AlgebraicError::InvalidSketchCapacity)?;
        if target > aggregate_cap {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
        while total < target {
            let mut advanced = false;
            for request in &mut requests {
                if total == target {
                    break;
                }
                if request.capacity < MAX_BUCKET_SKETCH_CAPACITY {
                    request.capacity = request
                        .capacity
                        .checked_add(1)
                        .ok_or(AlgebraicError::InvalidSketchCapacity)?;
                    total = total
                        .checked_add(1)
                        .ok_or(AlgebraicError::InvalidSketchCapacity)?;
                    advanced = true;
                }
            }
            if !advanced {
                return Err(AlgebraicError::InvalidSketchCapacity);
            }
        }
    }
    Ok(ProvisionedBuckets {
        requests,
        truncated: false,
    })
}

/// Parses and independently decodes concatenated residual bucket sketches.
///
/// Requests must be strictly ordered by ascending bucket ID. Each requested
/// sketch is serialized as little-endian syndrome coordinates with no length
/// prefix; the request capacities define the boundaries.
///
/// Structural and budget errors abort the batch. A normal decode failure is
/// isolated to its bucket so successfully decoded roots can be retained.
///
/// # Errors
/// Returns an error for invalid ordering, capacity, aggregate or byte length,
/// or when a decoder exceeds its work budget.
pub fn decode_bucket_sketches(
    encoded: &[u8],
    requests: &[BucketRequest],
) -> Result<BucketDecodeBatch, AlgebraicError> {
    validate_bucket_requests(requests)?;

    let mut offset = 0_usize;
    let mut successful_buckets = Vec::new();
    let mut failed_buckets = Vec::new();
    for request in requests {
        let byte_len = request
            .capacity
            .checked_mul(8)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        let end = offset
            .checked_add(byte_len)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        let bytes = encoded
            .get(offset..end)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        offset = end;

        let coordinates = bytes
            .chunks_exact(8)
            .map(|coordinate| {
                let mut value = [0; 8];
                value.copy_from_slice(coordinate);
                u64::from_le_bytes(value)
            })
            .collect();
        let sketch = SyndromeSketch::from_coordinates(coordinates)?;
        match sketch.decode_elements(request.capacity) {
            Ok(roots) => successful_buckets.push(BucketDecodeSuccess {
                depth: request.depth,
                prefix: request.prefix,
                roots,
            }),
            Err(AlgebraicError::DecodeFailure) => failed_buckets.push(request.prefix),
            Err(error) => return Err(error),
        }
    }
    if offset != encoded.len() {
        return Err(AlgebraicError::InvalidSketchLength);
    }
    Ok(BucketDecodeBatch {
        successful_buckets,
        failed_buckets,
    })
}

fn validate_bucket_requests(requests: &[BucketRequest]) -> Result<(), AlgebraicError> {
    let mut total_capacity = 0_usize;
    let mut previous_end_inclusive: Option<u64> = None;
    for request in requests {
        if request.capacity == 0 || request.capacity > MAX_BUCKET_SKETCH_CAPACITY {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
        if request.depth >= 32 || request.prefix >= (1_u32 << request.depth) {
            return Err(AlgebraicError::InvalidBucketIndex);
        }

        let shift = 64_u8.saturating_sub(request.depth);
        let start = if shift == 64 {
            0
        } else {
            u64::from(request.prefix) << shift
        };
        let end_inclusive = if shift == 64 {
            u64::MAX
        } else {
            start | (1_u64 << shift).wrapping_sub(1)
        };

        if let Some(prev_end) = previous_end_inclusive {
            if start <= prev_end {
                return Err(AlgebraicError::InvalidBucketIndex);
            }
        }
        previous_end_inclusive = Some(end_inclusive);

        total_capacity = total_capacity
            .checked_add(request.capacity)
            .ok_or(AlgebraicError::InvalidSketchCapacity)?;
        if total_capacity > MAX_BUCKETED_SKETCH_CAPACITY {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::reconcile::{ElementHash, ResidentKernel};

    fn toggle_stratum(strata: &mut [[u64; STRATUM_CAPACITY]; STRATA_COUNT], value: u64) {
        let event = ElementHash {
            h128: u128::from(value),
            h64: value,
        };
        let mut resident = ResidentKernel::new();
        resident.insert(event).unwrap();
        for (target, source) in strata.iter_mut().zip(resident.strata()) {
            for (coordinate, value) in target.iter_mut().zip(source) {
                *coordinate ^= value;
            }
        }
    }

    #[test]
    fn strata_tail_is_exact_when_every_stratum_decodes() {
        let local = [[0; STRATUM_CAPACITY]; STRATA_COUNT];
        let mut remote = local;
        for value in [1, 2, 4, 8, 3, 5] {
            toggle_stratum(&mut remote, value);
        }
        assert_eq!(estimate_delta(&local, &remote), Ok(Some(6)));
        assert_eq!(estimate_delta(&local, &local), Ok(Some(0)));
    }

    #[test]
    fn empty_sparse_tail_does_not_claim_large_difference_is_zero() {
        let local = [[0; STRATUM_CAPACITY]; STRATA_COUNT];
        let mut remote = local;
        for value in (1..=17).step_by(2) {
            toggle_stratum(&mut remote, value);
        }
        assert_eq!(estimate_delta(&local, &remote), Ok(None));
    }

    #[test]
    fn bucket_selection_checks_shape_and_exact_counts() {
        let local = vec![ResidentBucket::default(); BUCKET_COUNT];
        let mut remote = vec![
            RemoteBucketSummary {
                accumulator: 0,
                count: 0,
            };
            BUCKET_COUNT
        ];
        remote[7] = RemoteBucketSummary {
            accumulator: 9,
            count: 3,
        };
        assert_eq!(
            select_differing_buckets(&local, &remote),
            Ok(vec![BucketDifference {
                bucket_id: 7,
                count_delta: 3,
            }])
        );
        assert_eq!(
            select_differing_buckets(&local[..BUCKET_COUNT - 1], &remote),
            Err(AlgebraicError::InvalidSketchLength)
        );

        let mut dirty = local;
        dirty[0].scan_required = true;
        assert_eq!(
            select_differing_buckets(&dirty, &remote),
            Err(AlgebraicError::CountOverflow)
        );
    }

    #[test]
    fn bucket_provisioning_is_bounded_and_rejects_aggregate_overflow() {
        let differences = [
            BucketDifference {
                bucket_id: 1,
                count_delta: 0,
            },
            BucketDifference {
                bucket_id: 2,
                count_delta: 20,
            },
        ];
        assert_eq!(
            provision_bucket_capacities(&differences, None, 64),
            Ok(ProvisionedBuckets {
                requests: vec![
                    BucketRequest {
                        depth: 8,
                        prefix: 1,
                        capacity: 19,
                    },
                    BucketRequest {
                        depth: 8,
                        prefix: 2,
                        capacity: 34,
                    },
                ],
                truncated: false,
            })
        );
        assert_eq!(
            provision_bucket_capacities(&differences, None, 32),
            Err(AlgebraicError::InvalidSketchCapacity)
        );
    }

    #[test]
    fn global_estimate_inflates_cancelled_count_residuals() {
        let differences = [
            BucketDifference {
                bucket_id: 1,
                count_delta: 0,
            },
            BucketDifference {
                bucket_id: 2,
                count_delta: 0,
            },
        ];
        assert_eq!(
            provision_bucket_capacities(&differences, Some(20), 64),
            Ok(ProvisionedBuckets {
                requests: vec![
                    BucketRequest {
                        depth: 8,
                        prefix: 1,
                        capacity: 19,
                    },
                    BucketRequest {
                        depth: 8,
                        prefix: 2,
                        capacity: 19,
                    },
                ],
                truncated: false,
            })
        );
        assert_eq!(
            provision_bucket_capacities(&differences, Some(100), 128),
            Err(AlgebraicError::InvalidSketchCapacity)
        );
    }

    #[test]
    fn bucket_provisioning_pads_capacity_when_deep_splitting() {
        // We need an estimate large enough to force depth_shift > 0.
        // est > 768 will do it (e.g., 800).
        let estimate = 800;
        let mut differences = Vec::new();
        // Use enough differing buckets so that max total capacity (N * 2 * 64) >= target (1204)
        for i in 0..10 {
            differences.push(BucketDifference {
                bucket_id: i,
                count_delta: 0,
            });
        }
        
        let provisioned = provision_bucket_capacities(&differences, Some(estimate), 4096).unwrap();
        
        // Target capacity for estimate 800 is 800 + 400 + 0 + 4 = 1204.
        let total_capacity: usize = provisioned.requests.iter().map(|req| req.capacity).sum();
        assert_eq!(total_capacity, 1204);
        
        // The requests should be padded evenly up from 8.
        for req in &provisioned.requests {
            assert!(req.capacity >= 60 && req.capacity <= 61);
            assert_eq!(req.depth, 9);
        }
    }

    #[test]
    fn bucket_provisioning_fails_padding_when_max_capacity_reached() {
        // Same as above, but with only 1 bucket. Max capacity is 2 * 64 = 128.
        // Target is 1204, which cannot be met, triggering the !advanced failure.
        let estimate = 800;
        let differences = [BucketDifference {
            bucket_id: 1,
            count_delta: 0,
        }];
        assert_eq!(
            provision_bucket_capacities(&differences, Some(estimate), 4096),
            Err(AlgebraicError::InvalidSketchCapacity)
        );
    }

    #[test]
    fn bucket_decoder_retains_successes_and_isolates_decode_failures() {
        let requests = [
            BucketRequest {
                depth: 8,
                prefix: 1,
                capacity: 2,
            },
            BucketRequest {
                depth: 8,
                prefix: 9,
                capacity: 2,
            },
        ];
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
            decode_bucket_sketches(&encoded, &requests),
            Ok(BucketDecodeBatch {
                successful_buckets: vec![BucketDecodeSuccess {
                    depth: 8,
                    prefix: 1,
                    roots: vec![7],
                }],
                failed_buckets: vec![9],
            })
        );
    }

    #[test]
    fn bucket_decoder_rejects_order_and_length_mismatches() {
        let unordered = [
            BucketRequest {
                depth: 8,
                prefix: 2,
                capacity: 1,
            },
            BucketRequest {
                depth: 8,
                prefix: 1,
                capacity: 1,
            },
        ];
        assert_eq!(
            decode_bucket_sketches(&[0; 16], &unordered),
            Err(AlgebraicError::InvalidBucketIndex)
        );
        assert_eq!(
            decode_bucket_sketches(
                &[0; 7],
                &[BucketRequest {
                    depth: 8,
                    prefix: 1,
                    capacity: 1,
                }],
            ),
            Err(AlgebraicError::InvalidSketchLength)
        );
        assert_eq!(
            decode_bucket_sketches(
                &[0; 9],
                &[BucketRequest {
                    depth: 8,
                    prefix: 1,
                    capacity: 1,
                }],
            ),
            Err(AlgebraicError::InvalidSketchLength)
        );
    }
}
