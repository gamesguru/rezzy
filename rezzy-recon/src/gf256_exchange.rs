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
        self.pending.clear();
        self.pending.push_back(BucketRequest::new(0, 0, capacity));
        self.drain_round()
    }

    /// Verifies a decoded round against both sides' node summaries and
    /// advances. `requests`, `local` and `remote` are index-aligned with the
    /// round that produced `batch`; `is_local` is exact local membership.
    ///
    /// # Errors
    /// Returns an error if the inputs are misaligned or name a node that was
    /// never requested.
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
        let (batch, verified) = verify_gf256_batch(batch, requests, local, remote, is_local)?;
        for node in verified {
            self.local_only.extend_from_slice(node.local_only());
            self.remote_only.extend_from_slice(node.remote_only());
        }
        for (depth, prefix) in batch.failed_buckets {
            match retry_failed_bucket(requests, depth, prefix, 0, false, MAX_ROUND_JUMP) {
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
    use crate::{
        decode_gf256_bucket_sketches, FixedWindowElementHash, FixedWindowPopulation, Gf256Decoder,
    };

    fn digest(seed: u64) -> [u8; 32] {
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

    fn population(seeds: impl Iterator<Item = u64>) -> FixedWindowPopulation {
        FixedWindowPopulation::new(
            seeds
                .map(|s| FixedWindowElementHash::from_digest32(digest(s)))
                .collect(),
        )
        .unwrap()
    }

    /// Local {0..400}, remote {150..550}: 150 local-only, 150 remote-only.
    /// Far above one bucket's capacity, so recovery requires splitting.
    #[test]
    fn splits_buckets_and_recovers_every_full_digest() {
        let local = population(0..400);
        let remote = population(150..550);
        let local_set: Vec<[u8; 32]> = (0..400).map(digest).collect();
        let decoder = Gf256Decoder::build().unwrap();
        let mut exchange = Gf256BucketExchange::default();
        let mut action = exchange.start(32);
        let mut rounds = 0;
        let mut saw_split = false;
        let (local_only, remote_only) = loop {
            match action {
                Gf256Action::Request(requests) => {
                    rounds += 1;
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
                    let batch =
                        decode_gf256_bucket_sketches(&diff, &requests, 3_000_000_000, &decoder)
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
                } => break (local_only, remote_only),
                Gf256Action::Fallback => panic!("fell back"),
            }
        };
        assert!(saw_split && rounds >= 2, "rounds={rounds}");
        let mut expect_local: Vec<_> = (0..150).map(digest).collect();
        let mut expect_remote: Vec<_> = (400..550).map(digest).collect();
        expect_local.sort_unstable();
        expect_remote.sort_unstable();
        assert_eq!(local_only, expect_local);
        assert_eq!(remote_only, expect_remote);
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
        let batch = decode_gf256_bucket_sketches(&diff, &requests, 1_000_000, &decoder).unwrap();
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
}
