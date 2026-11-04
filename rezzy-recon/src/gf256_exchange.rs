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

use crate::client::{retry_failed_bucket, MAX_BUCKETS_PER_ROUND, MAX_RECONCILIATION_ROUNDS};
use crate::triage::{
    bucket_range_start, validate_adaptive_bucket_requests, BucketRequest, Gf256BucketDecodeBatch,
    NodeSummary, MAX_BUCKETED_SKETCH_CAPACITY,
};
use crate::verify::verify_gf256_batch;
use crate::{AlgebraicError, Gf256Sketch};

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
    /// The exchange cannot finish within its limits; use the baseline.
    Fallback,
}

/// Bucket exchange planner for the GF(2^256) path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gf256BucketExchange {
    pending: VecDeque<BucketRequest>,
    rounds_emitted: usize,
    max_rounds: usize,
    max_buckets_per_round: usize,
    max_aggregate_capacity: usize,
    local_only: Vec<[u8; 32]>,
    remote_only: Vec<[u8; 32]>,
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
            rounds_emitted: 0,
            max_rounds,
            max_buckets_per_round,
            max_aggregate_capacity,
            local_only: Vec::new(),
            remote_only: Vec::new(),
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
    /// Returns an error if the initial request set is malformed or exceeds
    /// the protocol's aggregate capacity.
    pub fn start_with_requests(
        &mut self,
        requests: Vec<BucketRequest>,
    ) -> Result<Gf256Action, AlgebraicError> {
        validate_adaptive_bucket_requests(&requests)?;
        self.reset();
        self.pending.extend(requests);
        Ok(self.drain_round())
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.rounds_emitted = 0;
        self.local_only.clear();
        self.remote_only.clear();
    }

    /// Verifies a decoded round against both sides' node summaries and
    /// advances. `requests`, `local` and `remote` are index-aligned with the
    /// round that produced `batch`; `is_local` is exact local membership.
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
        if !bucket_batch_covers_requests(&batch, requests) {
            return Err(AlgebraicError::InvalidBucketIndex);
        }
        let (batch, verified) = verify_gf256_batch(batch, requests, local, remote, is_local)?;
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
            // symmetric-difference cardinality. Use it to size a retry; when
            // it already exceeds the per-bucket ceiling, split immediately.
            let count_delta = local[index].count.abs_diff(remote[index].count);
            let force_split = count_delta > crate::triage::MAX_BUCKET_SKETCH_CAPACITY as u64;
            match retry_failed_bucket(
                requests,
                depth,
                prefix,
                count_delta,
                force_split,
                MAX_ROUND_JUMP,
            ) {
                Ok(children) => self.pending.extend(children),
                Err(_) => return Ok(Gf256Action::Fallback),
            }
        }
        self.pending
            .make_contiguous()
            .sort_unstable_by_key(bucket_range_start);
        if self.pending.is_empty() {
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

    fn drain_round(&mut self) -> Gf256Action {
        if self.rounds_emitted >= self.max_rounds {
            return Gf256Action::Fallback;
        }
        let mut total = 0_usize;
        let mut round = Vec::new();
        while let Some(candidate) = self.pending.front().copied() {
            let Some(next_total) = total.checked_add(candidate.capacity) else {
                return Gf256Action::Fallback;
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
            return Gf256Action::Fallback;
        }
        self.rounds_emitted = self.rounds_emitted.saturating_add(1);
        Gf256Action::Request(round)
    }
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
                Gf256Action::Fallback => panic!("fell back"),
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
    }

    #[test]
    fn count_difference_above_capacity_splits_without_retry() {
        let request = BucketRequest::new(0, 0, 32);
        let batch = Gf256BucketDecodeBatch {
            successful_buckets: Vec::new(),
            failed_buckets: alloc::vec![(0, 0)],
        };
        let local = [NodeSummary {
            count: 80,
            digest: 0,
        }];
        let remote = [NodeSummary {
            count: 0,
            digest: 0,
        }];
        let action = Gf256BucketExchange::default()
            .advance_verified(batch, &[request], &local, &remote, |_| false)
            .unwrap();
        let Gf256Action::Request(children) = action else {
            panic!("count lower bound should lead to child buckets");
        };
        assert!(children
            .iter()
            .all(|child| { child.depth > request.depth && child.capacity <= 32 }));
    }

    #[test]
    fn round_budget_exhaustion_falls_back() {
        let mut exchange = Gf256BucketExchange::new(0, 8, 4096);
        assert_eq!(exchange.start(8), Gf256Action::Fallback);
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
            Gf256BucketExchange::default().advance_verified(
                batch,
                &requests,
                &summaries,
                &summaries,
                |_| true,
            )
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
}
