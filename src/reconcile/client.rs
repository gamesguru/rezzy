// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Requester-side MSC0501 reconciliation decisions and verification.

use super::{
    AlgebraicError, EventHash, MAX_LOCAL_SKETCH_DECODE_CAPACITY, RoomAccumulator, SyndromeSketch,
    verify_residual,
};

/// Requester policy for one MSC0501 reconciliation exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconciliationClient {
    max_sketch_capacity: usize,
}

/// Information learned from the responder's room digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteDigest {
    pub digest: u128,
    pub known_event_count: u64,
    /// Whether both digests cover the same frame anchors.
    pub frame_matches: bool,
    /// Whether the responder advertised an extremity unknown to the requester.
    pub has_unknown_extremity: bool,
}

/// The next request selected by the reconciliation client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAction {
    /// The frame digest and count agree; no request is needed.
    Synchronized,
    /// Locate a common DAG anchor before attempting set extraction.
    ExtremityDiff,
    /// Send an unbucketed syndrome sketch at the given capacity.
    Sketch {
        capacity: usize,
        include_bucket_summary: bool,
    },
}

impl Default for ReconciliationClient {
    fn default() -> Self {
        Self {
            max_sketch_capacity: MAX_LOCAL_SKETCH_DECODE_CAPACITY,
        }
    }
}

impl ReconciliationClient {
    /// Creates a requester with an explicit local unbucketed decode limit.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::InvalidSketchCapacity`] for a zero limit or a
    /// limit above the implementation's local decode policy.
    pub fn new(max_sketch_capacity: usize) -> Result<Self, AlgebraicError> {
        if max_sketch_capacity == 0 || max_sketch_capacity > MAX_LOCAL_SKETCH_DECODE_CAPACITY {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
        Ok(Self {
            max_sketch_capacity,
        })
    }

    /// Selects the next protocol action from local and remote level-0 state.
    ///
    /// `concurrency_headroom` accounts for events expected to arrive during
    /// the exchange. Capacity follows the MSC's `ceil(1.5 * count_delta) + 4`
    /// rule. Requests that exceed local policy are capped and ask for a bucket
    /// summary so the next exchange can localize the difference.
    #[must_use]
    pub fn select_action(
        self,
        local: RoomAccumulator,
        remote: RemoteDigest,
        concurrency_headroom: usize,
    ) -> ClientAction {
        if !remote.frame_matches || remote.has_unknown_extremity {
            return ClientAction::ExtremityDiff;
        }
        if local.digest() == remote.digest && local.known_event_count() == remote.known_event_count
        {
            return ClientAction::Synchronized;
        }

        let count_delta = local.known_event_count().abs_diff(remote.known_event_count);
        let provisioned = u64::try_from(concurrency_headroom)
            .ok()
            .and_then(|headroom| {
                count_delta
                    .checked_add(count_delta / 2)
                    .and_then(|capacity| capacity.checked_add(count_delta % 2))
                    .and_then(|capacity| capacity.checked_add(4))
                    .and_then(|capacity| capacity.checked_add(headroom))
            });
        let capacity = provisioned
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(usize::MAX);
        ClientAction::Sketch {
            capacity: capacity.min(self.max_sketch_capacity),
            include_bucket_summary: count_delta == 0 || capacity > self.max_sketch_capacity,
        }
    }

    /// Builds the requester's unbucketed sketch over the negotiated frame.
    ///
    /// # Errors
    /// Returns an error for an invalid capacity or a zero short identifier.
    pub fn build_sketch(
        self,
        capacity: usize,
        hashes: impl IntoIterator<Item = EventHash>,
    ) -> Result<SyndromeSketch, AlgebraicError> {
        if capacity == 0 || capacity > self.max_sketch_capacity {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
        let mut sketch = SyndromeSketch::new(capacity)?;
        for hash in hashes {
            sketch.toggle(hash.h64)?;
        }
        Ok(sketch)
    }

    /// Verifies a decoded two-sided difference against its level-0 residual.
    ///
    /// The caller resolves responder-side event IDs and requester-only short
    /// IDs to [`EventHash`] values before calling this method. A successful
    /// result authenticates accidental decode correctness, not a malicious
    /// peer; normal Matrix PDU verification remains mandatory.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::DecodeFailure`] when either the requester-side
    /// accumulator or the complete symmetric-difference residual disagrees.
    pub fn verify_difference(
        local: RoomAccumulator,
        remote_digest: u128,
        expected_requester_side_accumulator: u128,
        responder_only: &[EventHash],
        requester_only: &[(u64, EventHash)],
    ) -> Result<(), AlgebraicError> {
        if requester_only
            .iter()
            .any(|(short_id, hash)| *short_id != hash.h64)
        {
            return Err(AlgebraicError::DecodeFailure);
        }
        if !verify_residual(
            expected_requester_side_accumulator,
            requester_only.iter().map(|(_, hash)| *hash),
        ) {
            return Err(AlgebraicError::DecodeFailure);
        }
        let residual = local.digest() ^ remote_digest;
        verify_residual(
            residual,
            responder_only
                .iter()
                .copied()
                .chain(requester_only.iter().map(|(_, hash)| *hash)),
        )
        .then_some(())
        .ok_or(AlgebraicError::DecodeFailure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(wide: u128, short: u64) -> EventHash {
        EventHash {
            h128: wide,
            h64: short,
        }
    }

    fn accumulator(hashes: &[EventHash]) -> RoomAccumulator {
        let mut accumulator = RoomAccumulator::new();
        for hash in hashes {
            accumulator.insert(*hash).unwrap();
        }
        accumulator
    }

    #[test]
    fn selects_short_circuit_extremity_and_sketch_paths() {
        let local = accumulator(&[hash(1, 1), hash(2, 2)]);
        let client = ReconciliationClient::default();
        let matching = RemoteDigest {
            digest: local.digest(),
            known_event_count: 2,
            frame_matches: true,
            has_unknown_extremity: false,
        };
        assert_eq!(
            client.select_action(local, matching, 0),
            ClientAction::Synchronized
        );
        assert_eq!(
            client.select_action(
                local,
                RemoteDigest {
                    frame_matches: false,
                    ..matching
                },
                0,
            ),
            ClientAction::ExtremityDiff
        );
        assert_eq!(
            client.select_action(
                local,
                RemoteDigest {
                    digest: 7,
                    known_event_count: 6,
                    ..matching
                },
                2,
            ),
            ClientAction::Sketch {
                capacity: 12,
                include_bucket_summary: false,
            }
        );
    }

    #[test]
    fn caps_large_and_two_sided_differences_for_localization() {
        let client = ReconciliationClient::new(16).unwrap();
        let local = accumulator(&[hash(1, 1)]);
        assert_eq!(
            client.select_action(
                local,
                RemoteDigest {
                    digest: 2,
                    known_event_count: 1_000,
                    frame_matches: true,
                    has_unknown_extremity: false,
                },
                0,
            ),
            ClientAction::Sketch {
                capacity: 16,
                include_bucket_summary: true,
            }
        );
        assert_eq!(
            client.select_action(
                local,
                RemoteDigest {
                    digest: 2,
                    known_event_count: 1,
                    frame_matches: true,
                    has_unknown_extremity: false,
                },
                0,
            ),
            ClientAction::Sketch {
                capacity: 4,
                include_bucket_summary: true,
            }
        );
    }

    #[test]
    fn builds_a_decodable_local_sketch() {
        let hashes = [hash(1, 3), hash(2, 5)];
        let sketch = ReconciliationClient::default()
            .build_sketch(4, hashes)
            .unwrap();
        assert_eq!(sketch.decode_elements(4).unwrap().as_slice(), &[3, 5]);
    }

    #[test]
    fn verifies_both_sides_of_a_decoded_difference() {
        let common = hash(1, 1);
        let requester_only = hash(2, 2);
        let responder_only = hash(4, 4);
        let local = accumulator(&[common, requester_only]);
        let remote = accumulator(&[common, responder_only]);

        assert_eq!(
            ReconciliationClient::verify_difference(
                local,
                remote.digest(),
                requester_only.h128,
                &[responder_only],
                &[(requester_only.h64, requester_only)],
            ),
            Ok(())
        );
        assert_eq!(
            ReconciliationClient::verify_difference(
                local,
                remote.digest(),
                8,
                &[responder_only],
                &[(requester_only.h64, requester_only)],
            ),
            Err(AlgebraicError::DecodeFailure)
        );
        assert_eq!(
            ReconciliationClient::verify_difference(
                local,
                remote.digest(),
                requester_only.h128,
                &[responder_only],
                &[(9, requester_only)],
            ),
            Err(AlgebraicError::DecodeFailure)
        );
    }
}
