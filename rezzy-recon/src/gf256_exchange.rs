// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Client-side bucket exchange for full-digest GF(2^256) reconciliation.
//!
//! Each round the client requests a set of disjoint buckets, subtracts its own
//! sketches from the responder's, decodes the difference to full digests and
//! verifies every node exactly ([`crate::verify_gf256_batch`]). Failed nodes
//! are retried at a larger capacity or split. Because the decoded elements are
//! the digests themselves there is no collision tracking and no second phase:
//! a node is either exactly verified or retried.

use alloc::{collections::VecDeque, vec::Vec};

use crate::client::{
    prune_awaiting, record_split_summaries, retry_failed_bucket, take_complete_split_summaries,
    MAX_BUCKETS_PER_ROUND, MAX_RECONCILIATION_ROUNDS,
};
use crate::triage::{
    bucket_range_start, validate_adaptive_bucket_requests, BucketRequest, Gf256BucketDecodeBatch,
    NodeSummary, MAX_BUCKETED_SKETCH_CAPACITY,
};
use crate::verify::verify_gf256_batch;
use crate::{AlgebraicError, Gf256Sketch};

/// Why a [`Gf256BucketExchange`] stopped without completing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gf256FallbackReason {
    /// The configured request/response round limit was reached.
    RoundLimit,
    /// A split's child summaries did not reconstruct the parent summary.
    SplitInconsistent,
    /// A failed bucket could not be retried or split further.
    RetryExhausted,
    /// The pending split children did not form a complete cover of a parent.
    IncompleteSplit,
    /// Adding a request's capacity overflowed the round accounting.
    CapacityOverflow,
    /// The next round was empty or failed request validation.
    InvalidRoundPlan,
}

impl core::fmt::Display for Gf256FallbackReason {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::RoundLimit => "round limit",
            Self::SplitInconsistent => "split summaries inconsistent",
            Self::RetryExhausted => "bucket cannot be retried or split",
            Self::IncompleteSplit => "incomplete split cover",
            Self::CapacityOverflow => "capacity overflow",
            Self::InvalidRoundPlan => "empty or invalid round plan",
        };
        formatter.write_str(text)
    }
}

/// The next step of a [`Gf256BucketExchange`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gf256Action {
    /// Request these buckets next.
    Request(Vec<BucketRequest>),
    /// Every bucket verified; the difference is complete and exact.
    Complete {
        /// Digests present locally and absent remotely.
        local_only: Vec<[u8; 32]>,
        /// Digests absent locally; the peer must supply the event IDs.
        remote_only: Vec<[u8; 32]>,
    },
    /// The exchange cannot finish; use the baseline.
    Fallback {
        /// The specific limit or inconsistency that stopped the exchange.
        reason: Gf256FallbackReason,
    },
}

/// Bucket exchange planner for the GF(2^256) path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gf256BucketExchange {
    pending: VecDeque<BucketRequest>,
    last_emitted_round: Option<Vec<BucketRequest>>,
    rounds_emitted: usize,
    max_rounds: usize,
    max_buckets_per_round: usize,
    max_aggregate_capacity: usize,
    local_only: Vec<[u8; 32]>,
    remote_only: Vec<[u8; 32]>,
    /// Remote summaries of failed parents awaiting their split children.
    parents: Vec<(u8, u64, NodeSummary)>,
    /// Remote summaries of split descendants seen so far.
    awaiting: Vec<(u8, u64, NodeSummary)>,
}

impl Default for Gf256BucketExchange {
    fn default() -> Self {
        Self::new(
            MAX_RECONCILIATION_ROUNDS,
            MAX_BUCKETS_PER_ROUND,
            MAX_BUCKETED_SKETCH_CAPACITY,
        )
    }
}

impl Gf256BucketExchange {
    /// Creates an exchange with the given round and wire limits.
    #[must_use]
    pub const fn new(
        max_rounds: usize,
        max_buckets_per_round: usize,
        max_aggregate_capacity: usize,
    ) -> Self {
        Self {
            pending: VecDeque::new(),
            last_emitted_round: None,
            rounds_emitted: 0,
            max_rounds,
            max_buckets_per_round,
            max_aggregate_capacity,
            local_only: Vec::new(),
            remote_only: Vec::new(),
            parents: Vec::new(),
            awaiting: Vec::new(),
        }
    }

    /// Emits the first round: the whole key space as one root bucket of
    /// `capacity` coordinates.
    #[must_use]
    pub fn start(&mut self, capacity: usize) -> Gf256Action {
        self.reset();
        self.pending.push_back(BucketRequest::new(0, 0, capacity));
        self.drain_round()
    }

    /// Starts an exchange from a client-planned set of disjoint buckets.
    ///
    /// This connects summary-based client selection to the full-digest
    /// exchange. The caller should pass the requests from
    /// [`crate::Gf256ClientAction::BucketSketches`].
    ///
    /// # Errors
    /// Returns an error if the initial request set is malformed, exceeds the
    /// protocol's aggregate capacity, or does not cover the full key space.
    pub fn start_with_requests(
        &mut self,
        requests: Vec<BucketRequest>,
    ) -> Result<Gf256Action, AlgebraicError> {
        validate_adaptive_bucket_requests(&requests)?;
        if !requests_cover_full_prefix_space(&requests) {
            return Err(AlgebraicError::InvalidBucketIndex);
        }
        self.reset();
        self.pending.extend(requests);
        Ok(self.drain_round())
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.last_emitted_round = None;
        self.rounds_emitted = 0;
        self.local_only.clear();
        self.remote_only.clear();
        self.parents.clear();
        self.awaiting.clear();
    }

    /// Verifies a decoded round against both sides' node summaries and
    /// advances. `requests`, `local` and `remote` are index-aligned with the
    /// most recently emitted round that produced `batch`; stale or mismatched
    /// request lists are rejected. `is_local` is exact local membership.
    ///
    /// The batch must hold exactly one outcome (success or failure) per
    /// request; a missing, duplicate or unrequested node is rejected so the
    /// exchange can never complete with part of the key space unaccounted for.
    ///
    /// Failed nodes first retry at a larger capacity and then split. That is
    /// correct for the failures that actually occur: an over-capacity bucket
    /// is indistinguishable from a bad decode at this layer, and there are no
    /// hash collisions that capacity cannot fix. A node that fails verification
    /// after decoding is not a capacity problem, but it is vanishingly rare
    /// (the decoder re-encodes its roots), so the extra round is not worth a
    /// separate signal.
    ///
    /// # Errors
    /// Returns an error if the inputs are misaligned or the batch does not
    /// cover exactly the requested nodes.
    pub fn advance_verified<F>(
        &mut self,
        batch: Gf256BucketDecodeBatch,
        requests: &[BucketRequest],
        local: &[NodeSummary],
        remote: &[NodeSummary],
        is_local: F,
    ) -> Result<Gf256Action, AlgebraicError>
    where
        F: FnMut(&[u8; 32]) -> bool,
    {
        if self.last_emitted_round.as_deref() != Some(requests) {
            return Err(AlgebraicError::InvalidBucketIndex);
        }
        if !bucket_batch_covers_requests(&batch, requests) {
            return Err(AlgebraicError::InvalidBucketIndex);
        }
        if local.len() != requests.len() || remote.len() != requests.len() {
            return Err(AlgebraicError::InvalidSketchLength);
        }
        if self.split_summaries_are_inconsistent(requests, remote) {
            self.last_emitted_round = None;
            return Ok(Gf256Action::Fallback {
                reason: Gf256FallbackReason::SplitInconsistent,
            });
        }
        let (batch, verified) = verify_gf256_batch(batch, requests, local, remote, is_local)?;
        self.last_emitted_round = None;
        for node in verified {
            self.local_only.extend_from_slice(node.local_only());
            self.remote_only.extend_from_slice(node.remote_only());
        }
        for (depth, prefix) in batch.failed_buckets {
            let Some(index) = requests
                .iter()
                .position(|request| request.depth == depth && request.prefix == prefix)
            else {
                return Err(AlgebraicError::InvalidBucketIndex);
            };
            // The absolute count difference is a lower bound on the
            // symmetric-difference cardinality. When the counts are close, the
            // sum is used instead: equal counts can still hide a large
            // symmetric difference (for example, disjoint sets of equal size),
            // and a one-bit split would then exhaust the round budget.
            let local_count = local[index].count;
            let remote_count = remote[index].count;
            let count_delta = local_count.abs_diff(remote_count);
            let force_split = count_delta > crate::triage::MAX_BUCKET_SKETCH_CAPACITY as u64;
            let split_estimate = if count_delta <= crate::triage::MAX_BUCKET_SKETCH_CAPACITY as u64
            {
                local_count.saturating_add(remote_count)
            } else {
                count_delta
            };
            match retry_failed_bucket(
                requests,
                depth,
                prefix,
                split_estimate,
                force_split,
                split_jump(split_estimate),
            ) {
                Ok(children) => {
                    if children.iter().any(|child| child.depth > depth)
                        && !self
                            .parents
                            .iter()
                            .any(|&(parent_depth, parent_prefix, _)| {
                                (parent_depth, parent_prefix) == (depth, prefix)
                            })
                    {
                        self.parents.push((depth, prefix, remote[index]));
                    }
                    self.pending.extend(children);
                }
                Err(_) => {
                    return Ok(Gf256Action::Fallback {
                        reason: Gf256FallbackReason::RetryExhausted,
                    });
                }
            }
        }
        self.pending
            .make_contiguous()
            .sort_unstable_by_key(bucket_range_start);
        if self.pending.is_empty() {
            if !self.parents.is_empty() {
                return Ok(Gf256Action::Fallback {
                    reason: Gf256FallbackReason::IncompleteSplit,
                });
            }
            let mut local_only = core::mem::take(&mut self.local_only);
            let mut remote_only = core::mem::take(&mut self.remote_only);
            local_only.sort_unstable();
            remote_only.sort_unstable();
            return Ok(Gf256Action::Complete {
                local_only,
                remote_only,
            });
        }
        Ok(self.drain_round())
    }

    /// Returns true when the responder's summaries for a split do not
    /// reconstruct the parent summary. A split may fan out by several prefix
    /// bits and its descendants may arrive across rounds, so partial covers
    /// are retained until complete. Local summaries are trusted; only the
    /// responder's summaries are checked.
    fn split_summaries_are_inconsistent(
        &mut self,
        requests: &[BucketRequest],
        remote: &[NodeSummary],
    ) -> bool {
        record_split_summaries(&self.parents, &mut self.awaiting, requests, remote);
        for (_, parent_summary, descendant_summary, count_valid) in
            take_complete_split_summaries(&mut self.parents, &self.awaiting)
        {
            if !count_valid || descendant_summary != parent_summary {
                return true;
            }
        }
        prune_awaiting(&self.parents, &mut self.awaiting);
        false
    }

    fn drain_round(&mut self) -> Gf256Action {
        if self.rounds_emitted >= self.max_rounds {
            self.last_emitted_round = None;
            return Gf256Action::Fallback {
                reason: Gf256FallbackReason::RoundLimit,
            };
        }
        let mut total = 0_usize;
        let mut round = Vec::new();
        while let Some(candidate) = self.pending.front().copied() {
            let Some(next_total) = total.checked_add(candidate.capacity) else {
                return Gf256Action::Fallback {
                    reason: Gf256FallbackReason::CapacityOverflow,
                };
            };
            if round.len() >= self.max_buckets_per_round || next_total > self.max_aggregate_capacity
            {
                break;
            }
            total = next_total;
            self.pending.pop_front();
            round.push(candidate);
        }
        if round.is_empty() || validate_adaptive_bucket_requests(&round).is_err() {
            self.last_emitted_round = None;
            return Gf256Action::Fallback {
                reason: Gf256FallbackReason::InvalidRoundPlan,
            };
        }
        self.rounds_emitted = self.rounds_emitted.saturating_add(1);
        self.last_emitted_round = Some(round.clone());
        Gf256Action::Request(round)
    }
}

/// Returns whether ordered, disjoint requests tile the whole digest prefix
/// space from its first key through the exclusive end.
fn requests_cover_full_prefix_space(requests: &[BucketRequest]) -> bool {
    let Some(space_end) = 1_u128.checked_shl(u32::from(crate::MAX_DEPTH)) else {
        return false;
    };
    let mut next_start = 0_u128;
    for request in requests {
        let Some(remaining_depth) = crate::MAX_DEPTH.checked_sub(request.depth) else {
            return false;
        };
        let Some(width) = 1_u128.checked_shl(u32::from(remaining_depth)) else {
            return false;
        };
        if bucket_range_start(request) != next_start {
            return false;
        }
        let Some(end) = next_start.checked_add(width) else {
            return false;
        };
        next_start = end;
    }
    next_start == space_end
}

/// Whether `batch` holds exactly one outcome for every request and nothing
/// else.
fn bucket_batch_covers_requests(
    batch: &Gf256BucketDecodeBatch,
    requests: &[BucketRequest],
) -> bool {
    let mut outcomes: Vec<(u8, u64)> = batch
        .successful_buckets
        .iter()
        .map(|bucket| (bucket.depth, bucket.prefix))
        .chain(batch.failed_buckets.iter().copied())
        .collect();
    outcomes.sort_unstable();
    let mut expected: Vec<(u8, u64)> = requests.iter().map(|r| (r.depth, r.prefix)).collect();
    expected.sort_unstable();
    outcomes == expected
}

/// Maximum split fan-out per retry, in bits (matches the GF(2^64) planner).
const MAX_ROUND_JUMP: u8 = crate::client::MAX_BUCKET_ROUND_DEPTH;

/// Chooses the smallest fan-out that brings the estimated child difference
/// within half a bucket's capacity ceiling, leaving room for uneven child
/// distribution. Splitting by the maximum seven bits for an ordinary
/// over-capacity bucket creates up to 128 children where a handful are enough,
/// which can exhaust the round budget before any child is decoded.
fn split_jump(estimated_delta: u64) -> u8 {
    let target_child_delta = (crate::triage::MAX_BUCKET_SKETCH_CAPACITY / 2) as u64;
    let buckets = estimated_delta.div_ceil(target_child_delta).max(1);
    let bits = if buckets <= 1 {
        0
    } else {
        buckets.saturating_sub(1).ilog2().saturating_add(1)
    };
    u8::try_from(bits)
        .unwrap_or(MAX_ROUND_JUMP)
        .clamp(1, MAX_ROUND_JUMP)
}

/// Subtracts the local sketches from the responder's concatenated payload and
/// re-serializes the difference, ready for
/// [`crate::decode_gf256_bucket_sketches`].
///
/// # Errors
/// Returns an error if the payload length does not match the requests.
pub fn subtract_gf256_payload(
    local: &[Gf256Sketch],
    remote_payload: &[u8],
    requests: &[BucketRequest],
) -> Result<Vec<u8>, AlgebraicError> {
    if local.len() != requests.len() {
        return Err(AlgebraicError::InvalidSketchLength);
    }
    let mut offset = 0_usize;
    let mut difference = Vec::with_capacity(remote_payload.len());
    for (sketch, request) in local.iter().zip(requests) {
        let end = offset
            .checked_add(
                request
                    .capacity
                    .checked_mul(32)
                    .ok_or(AlgebraicError::InvalidSketchLength)?,
            )
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        let bytes = remote_payload
            .get(offset..end)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        offset = end;
        let remote = Gf256Sketch::from_bytes(request.capacity, bytes)?;
        difference.extend_from_slice(&sketch.subtract(&remote)?.to_bytes());
    }
    if offset != remote_payload.len() {
        return Err(AlgebraicError::InvalidSketchLength);
    }
    Ok(difference)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::{FixedWindowElementHash, FixedWindowPopulation, Gf256Decoder};

    use crate::test_util::digest32 as digest;

    fn population(seeds: impl Iterator<Item = u64>) -> FixedWindowPopulation {
        FixedWindowPopulation::new(
            seeds
                .map(|s| FixedWindowElementHash::from_digest32(digest(s)))
                .collect(),
        )
        .unwrap()
    }

    fn failed_root(
        local_count: u64,
        remote_count: u64,
    ) -> (
        BucketRequest,
        Gf256BucketDecodeBatch,
        [NodeSummary; 1],
        [NodeSummary; 1],
    ) {
        (
            BucketRequest::new(0, 0, 32),
            Gf256BucketDecodeBatch {
                successful_buckets: Vec::new(),
                failed_buckets: alloc::vec![(0, 0)],
            },
            [NodeSummary {
                count: local_count,
                digest: 0,
            }],
            [NodeSummary {
                count: remote_count,
                digest: 0,
            }],
        )
    }

    /// Runs a full exchange between two populations and returns
    /// `(local_only, remote_only, rounds, saw_split)`.
    fn run_exchange(
        local: &FixedWindowPopulation,
        remote: &FixedWindowPopulation,
        local_set: &[[u8; 32]],
        root_capacity: usize,
    ) -> (Vec<[u8; 32]>, Vec<[u8; 32]>, usize, bool) {
        let mut exchange = Gf256BucketExchange::default();
        let action = exchange.start(root_capacity);
        run_exchange_from_action(local, remote, local_set, exchange, action)
    }

    fn run_exchange_from_action(
        local: &FixedWindowPopulation,
        remote: &FixedWindowPopulation,
        local_set: &[[u8; 32]],
        mut exchange: Gf256BucketExchange,
        mut action: Gf256Action,
    ) -> (Vec<[u8; 32]>, Vec<[u8; 32]>, usize, bool) {
        let decoder = Gf256Decoder::build().unwrap();
        let mut rounds = 0_usize;
        let mut saw_split = false;
        loop {
            match action {
                Gf256Action::Request(requests) => {
                    rounds = rounds.saturating_add(1);
                    saw_split |= requests.iter().any(|r| r.depth > 0);
                    let remote_nodes = remote.bucket_nodes(&requests).unwrap();
                    let payload: Vec<u8> = remote_nodes
                        .iter()
                        .flat_map(|(sketch, _)| sketch.to_bytes())
                        .collect();
                    let remote_summaries: Vec<_> = remote_nodes.iter().map(|n| n.1).collect();
                    let local_nodes = local.bucket_nodes(&requests).unwrap();
                    let local_sketches: Vec<_> = local_nodes.iter().map(|n| n.0.clone()).collect();
                    let local_summaries: Vec<_> = local_nodes.iter().map(|n| n.1).collect();
                    let diff =
                        subtract_gf256_payload(&local_sketches, &payload, &requests).unwrap();
                    let batch = crate::decode_gf256_bucket_sketches_with_summaries(
                        &diff,
                        &requests,
                        &local_summaries,
                        &remote_summaries,
                        3_000_000_000,
                        &decoder,
                    )
                    .unwrap();
                    action = exchange
                        .advance_verified(
                            batch,
                            &requests,
                            &local_summaries,
                            &remote_summaries,
                            |d| local_set.contains(d),
                        )
                        .unwrap();
                }
                Gf256Action::Complete {
                    local_only,
                    remote_only,
                } => return (local_only, remote_only, rounds, saw_split),
                Gf256Action::Fallback { reason } => panic!("fell back: {reason}"),
            }
        }
    }

    /// Local {0..total}, remote {only..total+only}: `only` differences each
    /// way. More than 32 in total forces a split (a failed root first retries
    /// at the capacity ceiling of 32, which cannot hold them).
    fn assert_exchange_recovers(total: u64, only: u64, root_capacity: usize) -> (usize, bool) {
        let local = population(0..total);
        let remote = population(only..total.saturating_add(only));
        let local_set: Vec<[u8; 32]> = (0..total).map(digest).collect();
        let (local_only, remote_only, rounds, saw_split) =
            run_exchange(&local, &remote, &local_set, root_capacity);
        let mut expect_local: Vec<_> = (0..only).map(digest).collect();
        let mut expect_remote: Vec<_> = (total..total.saturating_add(only)).map(digest).collect();
        expect_local.sort_unstable();
        expect_remote.sort_unstable();
        assert_eq!(local_only, expect_local);
        assert_eq!(remote_only, expect_remote);
        (rounds, saw_split)
    }

    /// Capacity retry recovers 20 local-only and 20 remote-only digests.
    #[test]
    fn recovers_after_capacity_retry() {
        let (rounds, _) = assert_exchange_recovers(40, 20, 8);
        assert!(rounds >= 2, "rounds={rounds}");
    }

    /// Starting at maximum capacity forces the 40/40 case to split.
    #[test]
    fn splits_buckets_and_recovers_every_full_digest() {
        let (rounds, saw_split) =
            assert_exchange_recovers(80, 40, crate::triage::MAX_BUCKET_SKETCH_CAPACITY);
        assert!(
            saw_split && rounds >= 2,
            "saw_split={saw_split}, rounds={rounds}"
        );
    }

    #[test]
    fn strata_underestimate_still_splits_and_recovers_full_digest_difference() {
        use crate::{Gf256ClientAction, ReconciliationClient, RemoteDigest, ResidentKernel};

        let local = population(0..40);
        let remote = population(20..60);
        let local_set: Vec<[u8; 32]> = (0..40).map(digest).collect();
        let mut local_state = ResidentKernel::new();
        let mut remote_state = ResidentKernel::new();
        for seed in 0..40 {
            local_state
                .insert(FixedWindowElementHash::from_digest32(digest(seed)))
                .unwrap();
        }
        for seed in 20..60 {
            remote_state
                .insert(FixedWindowElementHash::from_digest32(digest(seed)))
                .unwrap();
        }
        let remote_summary = RemoteDigest {
            digest: remote_state.accumulator().digest(),
            known_event_count: remote_state.accumulator().known_event_count(),
            // Deliberately under-estimate: the actual sets differ by 40
            // elements, but the strata hint says zero.
            strata: *local_state.strata(),
            frame_matches: true,
            has_unknown_extremity: false,
        };
        let Gf256ClientAction::BucketSketches { requests } =
            ReconciliationClient::default().select_gf256_action(&local_state, remote_summary, 0)
        else {
            panic!("different summaries should select a bucket exchange");
        };
        assert!(requests
            .iter()
            .all(|request| request.capacity == crate::triage::MAX_BUCKET_SKETCH_CAPACITY));
        assert_eq!(requests, alloc::vec![BucketRequest::new(0, 0, 32)]);
        let mut exchange = Gf256BucketExchange::default();
        let first = exchange.start_with_requests(requests).unwrap();
        let (local_only, remote_only, rounds, _) =
            run_exchange_from_action(&local, &remote, &local_set, exchange, first);
        assert!(rounds >= 1);
        let mut expected_local: Vec<_> = (0..20).map(digest).collect();
        let mut expected_remote: Vec<_> = (40..60).map(digest).collect();
        expected_local.sort_unstable();
        expected_remote.sort_unstable();
        assert_eq!(local_only, expected_local);
        assert_eq!(remote_only, expected_remote);
    }

    #[test]
    fn strata_overestimate_stays_bounded_and_recovers_full_digest_difference() {
        use crate::{Gf256ClientAction, ReconciliationClient, RemoteDigest, ResidentKernel};

        let local = population(0..20);
        let remote = population(1..21);
        let local_set: Vec<[u8; 32]> = (0..20).map(digest).collect();
        let mut local_state = ResidentKernel::new();
        let mut remote_state = ResidentKernel::new();
        let mut inflated_strata = ResidentKernel::new();
        for seed in 0..20 {
            local_state
                .insert(FixedWindowElementHash::from_digest32(digest(seed)))
                .unwrap();
        }
        for seed in 1..21 {
            remote_state
                .insert(FixedWindowElementHash::from_digest32(digest(seed)))
                .unwrap();
        }
        for seed in 1..=256 {
            inflated_strata
                .insert(FixedWindowElementHash::from_digest32(digest(seed + 10_000)))
                .unwrap();
        }
        let remote_summary = RemoteDigest {
            digest: remote_state.accumulator().digest(),
            known_event_count: remote_state.accumulator().known_event_count(),
            strata: *inflated_strata.strata(),
            frame_matches: true,
            has_unknown_extremity: false,
        };
        let Gf256ClientAction::BucketSketches { requests } = ReconciliationClient::default()
            .allow_unlimited_delta()
            .select_gf256_action(&local_state, remote_summary, 0)
        else {
            panic!("over-estimated strata should remain a bounded bucket plan");
        };
        assert!(requests.len() > 1);
        assert!(requests
            .iter()
            .all(|request| request.capacity == crate::triage::MAX_BUCKET_SKETCH_CAPACITY));
        assert_eq!(
            requests.len(),
            1_usize << requests[0].depth,
            "requests must cover the entire key space at one depth"
        );
        let mut exchange = Gf256BucketExchange::default();
        let first = exchange.start_with_requests(requests).unwrap();
        let (local_only, remote_only, _, _) =
            run_exchange_from_action(&local, &remote, &local_set, exchange, first);
        assert_eq!(local_only, alloc::vec![digest(0)]);
        assert_eq!(remote_only, alloc::vec![digest(20)]);
    }

    /// The original 150/150 case: slow unoptimized, so run it in release.
    #[test]
    #[ignore = "stress: run with `cargo test --release -- --ignored`"]
    fn stress_splits_and_recovers_150_each_way() {
        let (rounds, saw_split) = assert_exchange_recovers(400, 150, 32);
        assert!(
            saw_split && rounds >= 2,
            "saw_split={saw_split}, rounds={rounds}"
        );
    }

    #[test]
    fn identical_sets_complete_in_one_round() {
        let both = population(0..50);
        let decoder = Gf256Decoder::build().unwrap();
        let mut exchange = Gf256BucketExchange::default();
        let Gf256Action::Request(requests) = exchange.start(8) else {
            panic!("expected a request");
        };
        let nodes = both.bucket_nodes(&requests).unwrap();
        let payload: Vec<u8> = nodes.iter().flat_map(|n| n.0.to_bytes()).collect();
        let sketches: Vec<_> = nodes.iter().map(|n| n.0.clone()).collect();
        let summaries: Vec<_> = nodes.iter().map(|n| n.1).collect();
        let diff = subtract_gf256_payload(&sketches, &payload, &requests).unwrap();
        let batch = crate::decode_gf256_bucket_sketches_with_summaries(
            &diff, &requests, &summaries, &summaries, 0, &decoder,
        )
        .unwrap();
        let action = exchange
            .advance_verified(batch, &requests, &summaries, &summaries, |_| true)
            .unwrap();
        assert_eq!(
            action,
            Gf256Action::Complete {
                local_only: Vec::new(),
                remote_only: Vec::new()
            }
        );
    }

    #[test]
    fn initial_planned_requests_are_validated_and_used() {
        let mut exchange = Gf256BucketExchange::default();
        let requests = alloc::vec![BucketRequest::new(1, 0, 32), BucketRequest::new(1, 1, 32)];
        assert_eq!(
            exchange.start_with_requests(requests.clone()).unwrap(),
            Gf256Action::Request(requests)
        );
        assert_eq!(
            exchange.start_with_requests(alloc::vec![BucketRequest::new(0, 0, 33)]),
            Err(AlgebraicError::InvalidSketchCapacity)
        );
        assert_eq!(
            exchange.start_with_requests(alloc::vec![BucketRequest::new(1, 0, 32)]),
            Err(AlgebraicError::InvalidBucketIndex),
            "a partial initial prefix cover cannot produce an exact result"
        );
    }

    #[test]
    fn count_difference_above_capacity_splits_without_retry() {
        let (request, batch, local, remote) = failed_root(80, 0);
        let mut exchange = Gf256BucketExchange::default();
        assert_eq!(
            exchange.start(32),
            Gf256Action::Request(alloc::vec![request])
        );
        let action = exchange
            .advance_verified(batch, &[request], &local, &remote, |_| false)
            .unwrap();
        let Gf256Action::Request(children) = action else {
            panic!("count lower bound should lead to child buckets");
        };
        assert_eq!(children.len(), 8, "80 differences need only eight children");
        assert_eq!(children[0].depth, 3);
        assert!(children
            .iter()
            .all(|child| { child.depth > request.depth && child.capacity <= 32 }));
    }

    #[test]
    fn balanced_counts_still_split_wide_enough() {
        let (request, batch, local, remote) = failed_root(100, 100);
        let mut exchange = Gf256BucketExchange::default();
        let _ = exchange.start(32);
        let action = exchange
            .advance_verified(batch, &[request], &local, &remote, |_| false)
            .unwrap();
        let Gf256Action::Request(children) = action else {
            panic!("equal counts must still split");
        };
        assert_eq!(children.len(), 16);
        assert_eq!(children[0].depth, 4);
        assert!(children
            .iter()
            .all(|child| child.depth > request.depth && child.capacity <= 32));
    }

    #[test]
    fn inconsistent_split_summaries_fall_back() {
        let (request, failed, local, remote) = failed_root(100, 0);
        let mut exchange = Gf256BucketExchange::default();
        assert_eq!(
            exchange.start(32),
            Gf256Action::Request(alloc::vec![request])
        );
        let Gf256Action::Request(children) = exchange
            .advance_verified(failed, &[request], &local, &remote, |_| false)
            .unwrap()
        else {
            panic!("parent should split");
        };
        let local_children = alloc::vec![NodeSummary::default(); children.len()];
        let remote_children = alloc::vec![
            NodeSummary {
                count: 1,
                digest: 0,
            };
            children.len()
        ];
        let batch = Gf256BucketDecodeBatch {
            successful_buckets: children
                .iter()
                .map(|child| crate::triage::Gf256BucketDecodeSuccess {
                    depth: child.depth,
                    prefix: child.prefix,
                    digests: Vec::new(),
                })
                .collect(),
            failed_buckets: Vec::new(),
        };
        assert_eq!(
            exchange
                .advance_verified(batch, &children, &local_children, &remote_children, |_| {
                    false
                },)
                .unwrap(),
            Gf256Action::Fallback {
                reason: Gf256FallbackReason::SplitInconsistent
            }
        );
    }

    #[test]
    fn round_budget_exhaustion_falls_back() {
        let mut exchange = Gf256BucketExchange::new(0, 8, 4096);
        assert_eq!(
            exchange.start(8),
            Gf256Action::Fallback {
                reason: Gf256FallbackReason::RoundLimit
            }
        );
    }

    #[test]
    fn request_above_aggregate_capacity_is_an_invalid_plan() {
        let mut exchange = Gf256BucketExchange::new(20, 8, 4);
        assert_eq!(
            exchange.start(8),
            Gf256Action::Fallback {
                reason: Gf256FallbackReason::InvalidRoundPlan
            }
        );
    }

    #[test]
    fn depth_limit_falls_back_with_retry_exhausted() {
        let request = BucketRequest::new(crate::MAX_DEPTH, 0, 32);
        let mut exchange = Gf256BucketExchange::new(20, 8, 4096);
        exchange.last_emitted_round = Some(alloc::vec![request]);
        let batch = Gf256BucketDecodeBatch {
            successful_buckets: Vec::new(),
            failed_buckets: alloc::vec![(crate::MAX_DEPTH, 0)],
        };
        let summaries = [NodeSummary {
            count: 1,
            digest: 0,
        }];
        assert_eq!(
            exchange
                .advance_verified(batch, &[request], &summaries, &summaries, |_| false)
                .unwrap(),
            Gf256Action::Fallback {
                reason: Gf256FallbackReason::RetryExhausted
            }
        );
    }

    #[test]
    fn missing_split_children_fall_back_as_incomplete() {
        let request = BucketRequest::new(0, 0, 32);
        let mut exchange = Gf256BucketExchange::new(20, 8, 4096);
        exchange.last_emitted_round = Some(alloc::vec![request]);
        exchange.parents.push((
            0,
            0,
            NodeSummary {
                count: 0,
                digest: 0,
            },
        ));
        let batch = Gf256BucketDecodeBatch {
            successful_buckets: alloc::vec![crate::triage::Gf256BucketDecodeSuccess {
                depth: 0,
                prefix: 0,
                digests: Vec::new(),
            }],
            failed_buckets: Vec::new(),
        };
        let summaries = [NodeSummary {
            count: 0,
            digest: 0,
        }];
        assert_eq!(
            exchange
                .advance_verified(batch, &[request], &summaries, &summaries, |_| false)
                .unwrap(),
            Gf256Action::Fallback {
                reason: Gf256FallbackReason::IncompleteSplit
            }
        );
    }

    #[test]
    fn capacity_sum_overflow_falls_back() {
        let mut exchange = Gf256BucketExchange::new(20, 8, usize::MAX);
        exchange
            .pending
            .push_back(BucketRequest::new(0, 0, usize::MAX));
        exchange.pending.push_back(BucketRequest::new(1, 0, 1));
        assert_eq!(
            exchange.drain_round(),
            Gf256Action::Fallback {
                reason: Gf256FallbackReason::CapacityOverflow
            }
        );
    }

    #[test]
    fn payload_length_is_checked() {
        let requests = [BucketRequest::new(0, 0, 2)];
        let local = [Gf256Sketch::new(2).unwrap()];
        assert!(subtract_gf256_payload(&local, &[0; 63], &requests).is_err());
        assert!(subtract_gf256_payload(&local, &[0; 65], &requests).is_err());
        assert!(subtract_gf256_payload(&local, &[0; 64], &requests).is_ok());
    }

    #[test]
    fn incomplete_duplicate_or_extra_outcomes_are_rejected() {
        use crate::triage::Gf256BucketDecodeSuccess;
        let requests = [BucketRequest::new(1, 0, 4), BucketRequest::new(1, 1, 4)];
        let ok = |depth, prefix| Gf256BucketDecodeSuccess {
            depth,
            prefix,
            digests: Vec::new(),
        };
        let summaries = [NodeSummary {
            count: 0,
            digest: 0,
        }; 2];
        let run = |batch: Gf256BucketDecodeBatch| {
            let mut exchange = Gf256BucketExchange::default();
            exchange.start_with_requests(requests.to_vec()).unwrap();
            exchange.advance_verified(batch, &requests, &summaries, &summaries, |_| true)
        };
        // Complete: accepted.
        let full = Gf256BucketDecodeBatch {
            successful_buckets: alloc::vec![ok(1, 0), ok(1, 1)],
            failed_buckets: Vec::new(),
        };
        assert!(matches!(run(full), Ok(Gf256Action::Complete { .. })));
        // Missing node.
        let missing = Gf256BucketDecodeBatch {
            successful_buckets: alloc::vec![ok(1, 0)],
            failed_buckets: Vec::new(),
        };
        assert_eq!(run(missing), Err(AlgebraicError::InvalidBucketIndex));
        // Duplicate node.
        let duplicate = Gf256BucketDecodeBatch {
            successful_buckets: alloc::vec![ok(1, 0), ok(1, 0)],
            failed_buckets: Vec::new(),
        };
        assert_eq!(run(duplicate), Err(AlgebraicError::InvalidBucketIndex));
        // Both a success and a failure for one node.
        let both = Gf256BucketDecodeBatch {
            successful_buckets: alloc::vec![ok(1, 0), ok(1, 1)],
            failed_buckets: alloc::vec![(1, 1)],
        };
        assert_eq!(run(both), Err(AlgebraicError::InvalidBucketIndex));
        // Unrequested node.
        let extra = Gf256BucketDecodeBatch {
            successful_buckets: alloc::vec![ok(1, 0), ok(1, 1), ok(2, 0)],
            failed_buckets: Vec::new(),
        };
        assert_eq!(run(extra), Err(AlgebraicError::InvalidBucketIndex));
    }

    #[test]
    fn stale_round_response_is_rejected() {
        let mut exchange = Gf256BucketExchange::default();
        let root = [BucketRequest::new(0, 0, 32)];
        assert_eq!(
            exchange.start_with_requests(root.to_vec()).unwrap(),
            Gf256Action::Request(root.to_vec())
        );
        let failed = Gf256BucketDecodeBatch {
            successful_buckets: Vec::new(),
            failed_buckets: alloc::vec![(0, 0)],
        };
        let local = [NodeSummary {
            count: 40,
            digest: 0,
        }];
        let remote = [NodeSummary::default()];
        let next = exchange
            .advance_verified(failed.clone(), &root, &local, &remote, |_| false)
            .unwrap();
        let Gf256Action::Request(children) = next else {
            panic!("over-capacity root should emit a split round");
        };
        assert!(exchange
            .advance_verified(failed, &root, &local, &remote, |_| false)
            .is_err());
        assert_eq!(
            exchange.last_emitted_round.as_deref(),
            Some(children.as_slice())
        );
    }
}
