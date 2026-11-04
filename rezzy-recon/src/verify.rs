// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Per-node two-phase MSC4521 verification of a decoded symmetric difference.
//!
//! Every sketch node travels with a [`NodeSummary`] (count and `h128` XOR), so
//! each node is verified on its own: a failed node is split or discarded while
//! its siblings are admitted.
//!
//! Phase 1 ([`verify_decode`], or [`verify_batch`] over a round) runs at
//! decode time: it checks every root lies in the node, classifies roots as
//! local-only (`L`) or remote-only (`M`), and enforces the count identity.
//! Phase 2 ([`verify_follow_up`]) runs once the peer has returned the
//! identifiers for `M`: it checks them structurally and then against the
//! node's residual.
//!
//! Every failure is [`AlgebraicError::DecodeFailure`]. There is deliberately
//! no variant that blames the peer: a colliding or ambiguous root is
//! indistinguishable from misbehavior, and the caller's response is the same
//! either way -- discard the node's result and use the baseline.

use alloc::vec::Vec;

use super::client::ReconciliationClient;
use super::server::H64Index;
use super::triage::{
    BucketDecodeBatch, BucketDecodeSuccess, BucketRequest, Gf256BucketDecodeBatch,
    Gf256BucketDecodeSuccess, NodeSummary,
};
use super::{AlgebraicError, ElementHash, FixedWindowElementHash};

/// One node's decoded roots, classified by local `h64` presence.
///
/// Keyed by the node's `(depth, prefix)`, and [`verify_follow_up`] refuses a
/// follow-up for any other node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    depth: u8,
    prefix: u64,
    l_roots: Vec<u64>,
    m_roots: Vec<u64>,
    /// XOR of every local `h128` candidate under an `L` root.
    local_accumulated: u128,
    /// The summaries this node was verified against. The remote one is what
    /// the peer claimed, and seeds split consistency if the node is narrowed.
    local: NodeSummary,
    remote: NodeSummary,
}

impl Classified {
    /// The responder's summary of this node, as claimed in the exchange.
    #[must_use]
    pub const fn remote_summary(&self) -> NodeSummary {
        self.remote
    }

    /// `D_A xor D_B` for this node.
    #[must_use]
    pub const fn residual(&self) -> u128 {
        self.local.digest ^ self.remote.digest
    }

    /// The larger side's element count for the node.
    #[must_use]
    pub fn population(&self) -> u64 {
        self.local.count.max(self.remote.count)
    }

    /// The node's `(depth, prefix)`.
    #[must_use]
    pub const fn node(&self) -> (u8, u64) {
        (self.depth, self.prefix)
    }

    /// Roots present locally (the remote lacks them).
    #[must_use]
    pub fn l_roots(&self) -> &[u64] {
        &self.l_roots
    }

    /// Roots absent locally; their identifiers must come from the peer.
    #[must_use]
    pub fn m_roots(&self) -> &[u64] {
        &self.m_roots
    }
}

/// Phase 1 for one node: classify `roots` and enforce
/// `count_A - count_B = |L| - |M|`.
///
/// `local` and `remote` are the two sides' summaries of this node.
/// `local_candidates(root)` returns the `h128` of every local element whose
/// `h64` is `root` (the multi-valued map; empty means absent). When `M` is
/// empty the residual is checked immediately, provided no `L` root is
/// ambiguous. Roots must be distinct and fall inside the node.
///
/// # Errors
/// [`AlgebraicError::DecodeFailure`] on any failed check.
pub fn verify_decode<F>(
    request: &BucketRequest,
    local: NodeSummary,
    remote: NodeSummary,
    roots: &[u64],
    mut local_candidates: F,
) -> Result<Classified, AlgebraicError>
where
    F: FnMut(u64) -> Vec<u128>,
{
    // The decoder yields distinct roots; reject a caller that does not, since
    // a repeated root would be masked by the length check in phase 2.
    let mut distinct = roots.to_vec();
    distinct.sort_unstable();
    if distinct.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(AlgebraicError::DecodeFailure);
    }
    let bounds = H64Index::bounds_unchecked(request);
    if roots
        .iter()
        .any(|&root| !bounds.contains(&u128::from(root)))
    {
        return Err(AlgebraicError::DecodeFailure);
    }

    let mut l_roots = Vec::new();
    let mut m_roots = Vec::new();
    let mut local_accumulated = 0_u128;
    let mut ambiguous = false;
    for &root in roots {
        let candidates = local_candidates(root);
        if candidates.is_empty() {
            m_roots.push(root);
        } else {
            ambiguous |= candidates.len() > 1;
            local_accumulated = candidates.iter().fold(local_accumulated, |acc, h| acc ^ h);
            l_roots.push(root);
        }
    }
    m_roots.sort_unstable();

    // Widened to i128, so these subtractions cannot overflow; `checked_sub`
    // only satisfies the arithmetic lint and the `else` arm is dead code.
    let lhs = i128::from(local.count).checked_sub(i128::from(remote.count));
    let rhs = (l_roots.len() as i128).checked_sub(m_roots.len() as i128);
    let (Some(lhs), Some(rhs)) = (lhs, rhs) else {
        return Err(AlgebraicError::DecodeFailure);
    };
    if lhs != rhs {
        return Err(AlgebraicError::DecodeFailure);
    }

    let residual = local.digest ^ remote.digest;
    if m_roots.is_empty() && !ambiguous {
        ReconciliationClient::verify_global_residual(residual, &[local_accumulated], &[])?;
    }
    Ok(Classified {
        depth: request.depth,
        prefix: request.prefix,
        l_roots,
        m_roots,
        local_accumulated,
        local,
        remote,
    })
}

/// Phase 1 over one decoded round.
///
/// `requests`, `local` and `remote` are index-aligned. Each successfully
/// decoded node is verified; a node that fails verification is moved into
/// `failed_buckets`, so the exchange splits or retries it while its siblings
/// are admitted. Returns the batch to feed the exchange, the classified nodes
/// that passed, and each phase-1-rejected node as `(depth, prefix, roots)` so
/// the exchange can tell a repeated collision from a spurious decode.
///
/// # Errors
/// [`AlgebraicError::InvalidSketchLength`] if the slices are not aligned, and
/// [`AlgebraicError::InvalidBucketIndex`] if a decoded node was never
/// requested.
pub fn verify_batch<F>(
    batch: BucketDecodeBatch,
    requests: &[BucketRequest],
    local: &[NodeSummary],
    remote: &[NodeSummary],
    mut local_candidates: F,
) -> Result<VerifiedBatch, AlgebraicError>
where
    F: FnMut(u64) -> Vec<u128>,
{
    let BucketDecodeBatch {
        successful_buckets,
        failed_buckets,
    } = batch;
    let BatchVerificationResult {
        admitted,
        failed: failed_buckets,
        verified,
        rejected,
    } = verify_batch_outcomes(
        successful_buckets,
        failed_buckets,
        requests,
        local,
        remote,
        |request, local, remote, success| {
            verify_decode(
                request,
                local,
                remote,
                &success.roots,
                &mut local_candidates,
            )
        },
        |success| {
            let mut roots = success.roots.clone();
            roots.sort_unstable();
            Some((success.depth, success.prefix, roots))
        },
    )?;
    Ok((
        BucketDecodeBatch {
            successful_buckets: admitted,
            failed_buckets,
        },
        verified,
        rejected,
    ))
}

/// Result of [`verify_batch`]: the batch to feed the exchange, the nodes that
/// passed phase 1, and the nodes rejected by it with their decoded roots.
pub type VerifiedBatch = (BucketDecodeBatch, Vec<Classified>, Vec<(u8, u64, Vec<u64>)>);

trait DecodeOutcome {
    fn node(&self) -> (u8, u64);
}

impl DecodeOutcome for BucketDecodeSuccess {
    fn node(&self) -> (u8, u64) {
        (self.depth, self.prefix)
    }
}

impl DecodeOutcome for Gf256BucketDecodeSuccess {
    fn node(&self) -> (u8, u64) {
        (self.depth, self.prefix)
    }
}

struct BatchVerificationResult<S, V, R> {
    admitted: Vec<S>,
    failed: Vec<(u8, u64)>,
    verified: Vec<V>,
    rejected: Vec<R>,
}

fn verify_batch_outcomes<S, V, R>(
    successful: Vec<S>,
    mut failed: Vec<(u8, u64)>,
    requests: &[BucketRequest],
    local: &[NodeSummary],
    remote: &[NodeSummary],
    mut verify: impl FnMut(&BucketRequest, NodeSummary, NodeSummary, &S) -> Result<V, AlgebraicError>,
    mut rejected: impl FnMut(&S) -> Option<R>,
) -> Result<BatchVerificationResult<S, V, R>, AlgebraicError>
where
    S: DecodeOutcome,
{
    if requests.len() != local.len() || requests.len() != remote.len() {
        return Err(AlgebraicError::InvalidSketchLength);
    }
    let mut admitted = Vec::with_capacity(successful.len());
    let mut verified = Vec::with_capacity(successful.len());
    let mut rejected_outcomes = Vec::new();
    for outcome in successful {
        let node = outcome.node();
        let slot = requests
            .iter()
            .position(|request| (request.depth, request.prefix) == node)
            .ok_or(AlgebraicError::InvalidBucketIndex)?;
        let (Some(request), Some(&local), Some(&remote)) =
            (requests.get(slot), local.get(slot), remote.get(slot))
        else {
            return Err(AlgebraicError::InvalidBucketIndex);
        };
        match verify(request, local, remote, &outcome) {
            Ok(value) => {
                verified.push(value);
                admitted.push(outcome);
            }
            Err(AlgebraicError::DecodeFailure) => {
                failed.push(node);
                if let Some(value) = rejected(&outcome) {
                    rejected_outcomes.push(value);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(BatchVerificationResult {
        admitted,
        failed,
        verified,
        rejected: rejected_outcomes,
    })
}

/// One GF(2^256) node whose decoded digests passed exact verification.
///
/// Unlike [`Classified`] there is no follow-up phase: the digests are the
/// elements themselves, so membership and the full residual are checked at
/// decode time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gf256Verified {
    depth: u8,
    prefix: u64,
    local_only: Vec<[u8; 32]>,
    remote_only: Vec<[u8; 32]>,
}

impl Gf256Verified {
    /// The node's `(depth, prefix)`.
    #[must_use]
    pub const fn node(&self) -> (u8, u64) {
        (self.depth, self.prefix)
    }

    /// Digests present locally and absent remotely.
    #[must_use]
    pub fn local_only(&self) -> &[[u8; 32]] {
        &self.local_only
    }

    /// Digests absent locally; the peer must supply the matching event IDs.
    #[must_use]
    pub fn remote_only(&self) -> &[[u8; 32]] {
        &self.remote_only
    }
}

/// Exact verification of one node's decoded GF(2^256) digests.
///
/// `is_local(digest)` is exact membership in the local population. Checks:
/// no all-zero or repeated digest, every digest's placement key lies in the
/// node, `count_A - count_B = |L| - |M|`, and the node residual
/// `D_A xor D_B` equals the XOR of the residual windows of every digest.
///
/// # Errors
/// Returns [`AlgebraicError::InvalidBucketIndex`] or
/// [`AlgebraicError::InvalidSketchCapacity`] if `request` is malformed, or
/// [`AlgebraicError::DecodeFailure`] on any failed check.
pub fn verify_gf256_decode<F>(
    request: &BucketRequest,
    local: NodeSummary,
    remote: NodeSummary,
    digests: &[[u8; 32]],
    mut is_local: F,
) -> Result<Gf256Verified, AlgebraicError>
where
    F: FnMut(&[u8; 32]) -> bool,
{
    crate::triage::validate_adaptive_bucket_requests(core::slice::from_ref(request))?;
    let mut distinct = digests.to_vec();
    distinct.sort_unstable();
    if distinct.windows(2).any(|pair| pair[0] == pair[1]) || distinct.contains(&[0; 32]) {
        return Err(AlgebraicError::DecodeFailure);
    }
    let bounds = H64Index::bounds_unchecked(request);
    let mut local_only = Vec::new();
    let mut remote_only = Vec::new();
    let mut residual = 0_u128;
    for digest in digests {
        let element = FixedWindowElementHash::from_digest32(*digest);
        if !bounds.contains(&u128::from(element.bucket_key())) {
            return Err(AlgebraicError::DecodeFailure);
        }
        residual ^= element.residual();
        if is_local(digest) {
            local_only.push(*digest);
        } else {
            remote_only.push(*digest);
        }
    }
    local_only.sort_unstable();
    remote_only.sort_unstable();

    let lhs = i128::from(local.count).checked_sub(i128::from(remote.count));
    let rhs = i128::try_from(local_only.len())
        .ok()
        .zip(i128::try_from(remote_only.len()).ok())
        .and_then(|(l, m)| l.checked_sub(m));
    let (Some(lhs), Some(rhs)) = (lhs, rhs) else {
        return Err(AlgebraicError::DecodeFailure);
    };
    if lhs != rhs || residual != local.digest ^ remote.digest {
        return Err(AlgebraicError::DecodeFailure);
    }
    Ok(Gf256Verified {
        depth: request.depth,
        prefix: request.prefix,
        local_only,
        remote_only,
    })
}

/// Exact verification over one decoded GF(2^256) round.
///
/// `requests`, `local` and `remote` are index-aligned. A node that fails
/// verification moves to `failed_buckets` so the exchange can split or retry
/// it while its siblings are admitted.
///
/// # Errors
/// [`AlgebraicError::InvalidSketchLength`] if the slices are not aligned, and
/// [`AlgebraicError::InvalidBucketIndex`] if a decoded node was never
/// requested.
pub fn verify_gf256_batch<F>(
    batch: Gf256BucketDecodeBatch,
    requests: &[BucketRequest],
    local: &[NodeSummary],
    remote: &[NodeSummary],
    mut is_local: F,
) -> Result<(Gf256BucketDecodeBatch, Vec<Gf256Verified>), AlgebraicError>
where
    F: FnMut(&[u8; 32]) -> bool,
{
    let Gf256BucketDecodeBatch {
        successful_buckets,
        failed_buckets,
    } = batch;
    let BatchVerificationResult {
        admitted,
        failed: failed_buckets,
        verified,
        ..
    } = verify_batch_outcomes(
        successful_buckets,
        failed_buckets,
        requests,
        local,
        remote,
        |request, local, remote, success| {
            verify_gf256_decode(request, local, remote, &success.digests, &mut is_local)
        },
        |_| None::<()>,
    )?;
    Ok((
        Gf256BucketDecodeBatch {
            successful_buckets: admitted,
            failed_buckets,
        },
        verified,
    ))
}

/// Phase 2 for one node: check the identifiers the peer returned for `M`.
///
/// `returned` holds the canonical 32-byte digests `D(e)` computed locally from
/// the returned identifiers. This function derives `h64` and `h128` itself, so
/// peer-supplied hashes can never be passed in. Each must re-derive to a root
/// in `M`, each root in `M` must be covered exactly once, and the node's full
/// residual `D_A xor D_B = A(L) xor A(M)` must hold. A root with several local
/// candidates is resolved here by that residual, or the result is discarded.
///
/// # Errors
/// [`AlgebraicError::DecodeFailure`] on any failure, including a `request`
/// for a different `node` than `classified`; the caller must discard the node's
/// result and fall back.
pub fn verify_follow_up(
    classified: &Classified,
    node: (u8, u64),
    returned: &[[u8; 32]],
) -> Result<(), AlgebraicError> {
    if classified.node() != node || returned.len() != classified.m_roots.len() {
        return Err(AlgebraicError::DecodeFailure);
    }
    let returned: Vec<ElementHash> = returned
        .iter()
        .map(|digest| ElementHash::from_digest32(*digest))
        .collect();
    let mut covered = alloc::vec![false; classified.m_roots.len()];
    for element in &returned {
        let slot = classified
            .m_roots
            .binary_search(&element.h64)
            .map_err(|_| AlgebraicError::DecodeFailure)?;
        let seen = covered.get_mut(slot).ok_or(AlgebraicError::DecodeFailure)?;
        if core::mem::replace(seen, true) {
            return Err(AlgebraicError::DecodeFailure);
        }
    }
    // Equal lengths plus no repeat means every root is covered exactly once.
    let remote_hashes: Vec<u128> = returned.iter().map(|e| e.h128).collect();
    ReconciliationClient::verify_global_residual(
        classified.residual(),
        &[classified.local_accumulated],
        &remote_hashes,
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::triage::BucketDecodeSuccess;

    const fn el(h128: u128, h64: u64) -> ElementHash {
        ElementHash { h128, h64, q3: h64 }
    }

    /// A digest whose derived `h64`/`h128` are exactly the given values
    /// (`h64` must be non-zero).
    fn dig(e: ElementHash) -> [u8; 32] {
        let mut d = [0_u8; 32];
        d[..8].copy_from_slice(&e.h64.to_be_bytes());
        d[16..].copy_from_slice(&e.h128.to_be_bytes());
        d
    }

    fn fu(c: &Classified, returned: &[ElementHash]) -> Result<(), AlgebraicError> {
        let digests: Vec<[u8; 32]> = returned.iter().map(|e| dig(*e)).collect();
        verify_follow_up(c, (0, 0), &digests)
    }

    const fn remote(digest: u128, count: u64) -> NodeSummary {
        NodeSummary { count, digest }
    }

    fn vd<F: FnMut(u64) -> Vec<u128>>(
        local_digest: u128,
        local_count: u64,
        remote: &NodeSummary,
        roots: &[u64],
        lookup: F,
    ) -> Result<Classified, AlgebraicError> {
        verify_decode(
            &BucketRequest::new(0, 0, 8),
            NodeSummary {
                count: local_count,
                digest: local_digest,
            },
            *remote,
            roots,
            lookup,
        )
    }

    /// Local multi-valued map over a fixed element list.
    fn lookup(local: &[ElementHash]) -> impl FnMut(u64) -> Vec<u128> + '_ {
        move |root| {
            local
                .iter()
                .filter(|e| e.h64 == root)
                .map(|e| e.h128)
                .collect()
        }
    }

    fn digest(set: &[ElementHash]) -> u128 {
        set.iter().fold(0, |acc, e| acc ^ e.h128)
    }

    #[test]
    fn honest_difference_passes_both_phases() {
        let shared = el(0x10, 1);
        let l = el(0x20, 2);
        let m = el(0x40, 3);
        let local = [shared, l];
        let far = [shared, m];
        let c = vd(
            digest(&local),
            2,
            &remote(digest(&far), 2),
            &[2, 3],
            lookup(&local),
        )
        .unwrap();
        assert_eq!(c.l_roots(), &[2]);
        assert_eq!(c.m_roots(), &[3]);
        assert_eq!(fu(&c, &[m]), Ok(()));
    }

    #[test]
    fn empty_m_checks_the_residual_immediately() {
        let l = el(0x20, 2);
        let ok = vd(0x20, 1, &remote(0, 0), &[2], lookup(&[l]));
        assert!(ok.is_ok());
        // Residual disagrees with A(L): rejected in phase 1, before any fetch.
        let bad = vd(0x21, 1, &remote(0, 0), &[2], lookup(&[l]));
        assert_eq!(bad, Err(AlgebraicError::DecodeFailure));
    }

    /// A shared element whose h64 collides with a remote-only one: the root
    /// survives the sketch (remote holds it twice, local once), is present
    /// locally so lands in `L`, but truly belongs to `M`. The count identity
    /// rejects it in phase 1, before any fetch.
    #[test]
    fn shared_element_collision_misclassifies_and_fails_the_count_identity() {
        let shared = el(0xA, 7);
        let remote_only = el(0xB, 7);
        let local = [shared];
        let far = [shared, remote_only];
        let r = vd(
            digest(&local),
            1,
            &remote(digest(&far), 2),
            &[7],
            lookup(&local),
        );
        // count_A - count_B = -1, but |L| - |M| = 1.
        assert_eq!(r, Err(AlgebraicError::DecodeFailure));
    }

    fn honest() -> (Classified, ElementHash, ElementHash) {
        let l = el(0x20, 2);
        let m1 = el(0x40, 3);
        let m2 = el(0x80, 4);
        let local = [l];
        let far = [m1, m2];
        let c = vd(
            digest(&local),
            1,
            &remote(digest(&far), 2),
            &[2, 3, 4],
            lookup(&local),
        )
        .unwrap();
        (c, m1, m2)
    }

    #[test]
    fn l_root_substituted_into_the_follow_up_is_rejected() {
        let (c, m1, _) = honest();
        assert_eq!(
            fu(&c, &[m1, el(0x20, 2)]),
            Err(AlgebraicError::DecodeFailure)
        );
    }

    #[test]
    fn root_covered_twice_is_rejected() {
        let (c, m1, _) = honest();
        assert_eq!(fu(&c, &[m1, m1]), Err(AlgebraicError::DecodeFailure));
    }

    #[test]
    fn extra_returned_id_is_rejected() {
        let (c, m1, m2) = honest();
        assert_eq!(
            fu(&c, &[m1, m2, el(0x1, 3)]),
            Err(AlgebraicError::DecodeFailure)
        );
    }

    #[test]
    fn missing_m_root_is_rejected() {
        let (c, m1, _) = honest();
        assert_eq!(fu(&c, &[m1]), Err(AlgebraicError::DecodeFailure));
    }

    #[test]
    fn right_roots_wrong_h128_fails_the_residual() {
        let (c, _, m2) = honest();
        assert_eq!(
            fu(&c, &[el(0x41, 3), m2]),
            Err(AlgebraicError::DecodeFailure)
        );
    }

    /// Local holds two elements under one h64 (they cancel locally), remote
    /// holds a third. The root is ambiguous: phase 1 defers the residual, and
    /// phase 2 discards the result via the same error as any other failure.
    #[test]
    fn ambiguous_local_root_is_discarded_without_a_misbehavior_variant() {
        let x1 = el(0x1, 5);
        let x2 = el(0x2, 5);
        let y = el(0x4, 5);
        let local = [x1, x2];
        let far = [y];
        let c = vd(
            digest(&local),
            2,
            &remote(digest(&far), 1),
            &[5],
            lookup(&local),
        )
        .expect("phase 1 defers an ambiguous root");
        assert_eq!(c.l_roots(), &[5]);
        assert_eq!(fu(&c, &[]), Err(AlgebraicError::DecodeFailure));
    }

    #[test]
    fn duplicate_roots_are_rejected() {
        let l = el(0x20, 2);
        let r = vd(0x20, 1, &remote(0, 0), &[2, 2], lookup(&[l]));
        assert_eq!(r, Err(AlgebraicError::DecodeFailure));
    }

    #[test]
    fn root_outside_the_node_is_rejected() {
        // Depth-1 node with prefix 0 covers h64 < 2^63.
        let request = BucketRequest::new(1, 0, 8);
        let inside = verify_decode(
            &request,
            NodeSummary::default(),
            NodeSummary {
                count: 1,
                digest: 0x4,
            },
            &[5],
            |_| Vec::new(),
        );
        assert!(inside.is_ok());
        let outside = verify_decode(
            &request,
            NodeSummary::default(),
            NodeSummary {
                count: 1,
                digest: 0x4,
            },
            &[u64::MAX],
            |_| Vec::new(),
        );
        assert_eq!(outside, Err(AlgebraicError::DecodeFailure));
    }

    #[test]
    fn follow_up_for_another_node_is_rejected() {
        let (c, m1, m2) = honest();
        let digests = [dig(m1), dig(m2)];
        assert_eq!(
            verify_follow_up(&c, (1, 1), &digests),
            Err(AlgebraicError::DecodeFailure)
        );
    }

    /// One node fails its count identity and is moved to `failed_buckets`
    /// (so the exchange splits it) while its sibling is admitted.
    #[test]
    fn failed_node_is_split_while_sibling_is_admitted() {
        let requests = [BucketRequest::new(1, 0, 8), BucketRequest::new(1, 1, 8)];
        let high = 1_u64 << 63;
        // Node 0: honest remote-only root 5. Node 1: same shape but the remote
        // reports an extra element no root accounts for.
        let local = [NodeSummary::default(), NodeSummary::default()];
        let far = [
            NodeSummary {
                count: 1,
                digest: 0x4,
            },
            NodeSummary {
                count: 2,
                digest: 0x8,
            },
        ];
        let batch = BucketDecodeBatch {
            successful_buckets: vec![
                BucketDecodeSuccess {
                    depth: 1,
                    prefix: 0,
                    roots: vec![5],
                },
                BucketDecodeSuccess {
                    depth: 1,
                    prefix: 1,
                    roots: vec![high | 5],
                },
            ],
            failed_buckets: Vec::new(),
        };
        let (batch, classified, rejected) =
            verify_batch(batch, &requests, &local, &far, |_| Vec::new()).unwrap();
        assert_eq!(batch.successful_buckets.len(), 1);
        assert_eq!(batch.successful_buckets[0].prefix, 0);
        assert_eq!(batch.failed_buckets, vec![(1, 1)]);
        assert_eq!(rejected, vec![(1, 1, vec![high | 5])]);
        assert_eq!(classified.len(), 1);
        assert_eq!(classified[0].node(), (1, 0));
    }

    #[test]
    fn batch_rejects_misaligned_inputs_and_unrequested_nodes() {
        let requests = [BucketRequest::new(1, 0, 8)];
        let ok = |roots_prefix: u64| BucketDecodeBatch {
            successful_buckets: vec![BucketDecodeSuccess {
                depth: 1,
                prefix: roots_prefix,
                roots: Vec::new(),
            }],
            failed_buckets: Vec::new(),
        };
        assert_eq!(
            verify_batch(ok(0), &requests, &[], &[], |_| Vec::new()).unwrap_err(),
            AlgebraicError::InvalidSketchLength
        );
        let one = [NodeSummary::default()];
        assert_eq!(
            verify_batch(ok(1), &requests, &one, &one, |_| Vec::new()).unwrap_err(),
            AlgebraicError::InvalidBucketIndex
        );
    }

    /// Pins the MSC4521 "Node summary" test vector.
    #[test]
    fn node_summary_matches_the_spec_vector() {
        let summary = NodeSummary {
            count: 3,
            digest: 1,
        };
        let mut expected = [0_u8; 24];
        expected[15] = 1;
        expected[23] = 3;
        assert_eq!(summary.to_bytes(), expected);
    }

    #[test]
    fn node_summary_round_trips_and_rejects_bad_length() {
        let summary = NodeSummary::from_h128s(&[0x1, 0x2, 0x4]).unwrap();
        assert_eq!(
            summary,
            NodeSummary {
                count: 3,
                digest: 0x7
            }
        );
        assert_eq!(NodeSummary::from_bytes(&summary.to_bytes()), Ok(summary));
        assert_eq!(
            NodeSummary::from_bytes(&[0; 23]),
            Err(AlgebraicError::InvalidSketchLength)
        );
    }

    use crate::test_util::digest32 as gf_digest;

    fn gf_population(seeds: &[u64]) -> crate::FixedWindowPopulation {
        crate::FixedWindowPopulation::new(
            seeds
                .iter()
                .map(|s| FixedWindowElementHash::from_digest32(gf_digest(*s)))
                .collect(),
        )
        .unwrap()
    }

    /// Local {1..=6}, remote {4..=9}: L = {1,2,3}, M = {7,8,9}.
    fn gf_fixture() -> (BucketRequest, NodeSummary, NodeSummary, Vec<[u8; 32]>) {
        let request = BucketRequest::new(0, 0, 8);
        let local = gf_population(&[1, 2, 3, 4, 5, 6]);
        let remote = gf_population(&[4, 5, 6, 7, 8, 9]);
        let (local_sketch, local_summary) = local.bucket_nodes(&[request]).unwrap().remove(0);
        let (remote_sketch, remote_summary) = remote.bucket_nodes(&[request]).unwrap().remove(0);
        let decoder = crate::Gf256Decoder::build().unwrap();
        let digests = crate::Gf256Sketch::extract_symmetric_difference(
            &local_sketch,
            &remote_sketch,
            8,
            &decoder,
        )
        .unwrap();
        (request, local_summary, remote_summary, digests)
    }

    fn in_local(digest: &[u8; 32]) -> bool {
        (1..=6).any(|seed| gf_digest(seed) == *digest)
    }

    #[test]
    fn gf256_verify_classifies_and_checks_full_residual() {
        let (request, local, remote, digests) = gf_fixture();
        let node = verify_gf256_decode(&request, local, remote, &digests, in_local).unwrap();
        assert_eq!(node.node(), (0, 0));
        assert_eq!(node.local_only().len(), 3);
        assert_eq!(node.remote_only().len(), 3);
        assert!(node.remote_only().iter().all(|d| !in_local(d)));
    }

    #[test]
    fn gf256_verify_rejects_tampering() {
        let (request, local, remote, digests) = gf_fixture();
        // Wrong residual.
        let bad = NodeSummary {
            digest: remote.digest ^ 1,
            ..remote
        };
        assert_eq!(
            verify_gf256_decode(&request, local, bad, &digests, in_local),
            Err(AlgebraicError::DecodeFailure)
        );
        // Wrong count.
        let bad = NodeSummary {
            count: remote.count + 1,
            ..remote
        };
        assert_eq!(
            verify_gf256_decode(&request, local, bad, &digests, in_local),
            Err(AlgebraicError::DecodeFailure)
        );
        // Missing, repeated, and zero digests.
        assert!(verify_gf256_decode(&request, local, remote, &digests[1..], in_local).is_err());
        let mut repeated = digests.clone();
        repeated.push(digests[0]);
        assert!(verify_gf256_decode(&request, local, remote, &repeated, in_local).is_err());
        let mut zero = digests.clone();
        zero.push([0; 32]);
        assert!(verify_gf256_decode(&request, local, remote, &zero, in_local).is_err());
        // Digest outside the node.
        let narrow = BucketRequest::new(1, 0, 8);
        assert!(verify_gf256_decode(&narrow, local, remote, &digests, in_local).is_err());
    }

    #[test]
    fn gf256_verify_rejects_invalid_bucket_geometry() {
        let (valid, local, remote, digests) = gf_fixture();
        for invalid in [
            BucketRequest::new(crate::MAX_DEPTH + 1, 0, 8),
            BucketRequest::new(u8::MAX, 0, 8),
            BucketRequest::new(1, 2, 8),
        ] {
            assert_eq!(
                verify_gf256_decode(&invalid, local, remote, &digests, in_local),
                Err(AlgebraicError::InvalidBucketIndex)
            );
        }
        assert!(verify_gf256_decode(&valid, local, remote, &digests, in_local).is_ok());
    }

    #[test]
    fn gf256_verify_batch_moves_failed_nodes() {
        let (request, local, remote, digests) = gf_fixture();
        let good = Gf256BucketDecodeSuccess {
            depth: 0,
            prefix: 0,
            digests,
        };
        let batch = Gf256BucketDecodeBatch {
            successful_buckets: vec![good.clone()],
            failed_buckets: vec![],
        };
        let (out, verified) =
            verify_gf256_batch(batch, &[request], &[local], &[remote], in_local).unwrap();
        assert_eq!(out.successful_buckets, vec![good.clone()]);
        assert_eq!(verified.len(), 1);

        let bad = NodeSummary {
            digest: remote.digest ^ 1,
            ..remote
        };
        let batch = Gf256BucketDecodeBatch {
            successful_buckets: vec![good],
            failed_buckets: vec![],
        };
        let (out, verified) =
            verify_gf256_batch(batch, &[request], &[local], &[bad], in_local).unwrap();
        assert!(out.successful_buckets.is_empty() && verified.is_empty());
        assert_eq!(out.failed_buckets, vec![(0, 0)]);

        let batch = Gf256BucketDecodeBatch::default();
        assert_eq!(
            verify_gf256_batch(batch, &[request], &[], &[remote], in_local).unwrap_err(),
            AlgebraicError::InvalidSketchLength
        );
    }
}
