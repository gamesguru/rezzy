#![no_std]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! Minisketch reconciliation helpers (MSC4521).
//!
//! This crate is independent of the core state resolution engine. Its default
//! dependencies are `base64`, `sha2` and `rezzy-log`.
//!
//! The `logging` feature (on by default) emits MSC4521 reconciliation events
//! through `tracing`. Outside of test builds the crate does not install a
//! subscriber or write logs itself; test builds install a stderr subscriber.

#[cfg(feature = "std")]
extern crate std;

extern crate alloc;

pub use rezzy_log::{debug, error, info, trace, warn};

pub mod algebraic;
pub(crate) mod binary_field;
pub mod bitmap;
pub mod client;
pub mod gf256;
pub mod gf256_exchange;
mod gf256_simd;
pub mod gf64;
pub mod gf64_simd;
pub mod gf8;
mod pinsketch;
pub mod reed_solomon;
pub mod resident;
pub mod server;
#[cfg(test)]
mod test_util;
pub mod triage;
pub mod verify;

/// Identifier contract required by reconciliation graph traversal.
///
/// Reconciliation only needs owned queue entries, ordered membership, and a
/// display representation for hashing. This is intentionally independent of
/// the stronger `rezzy::EventId` contract.
pub trait EventId: Clone + Ord + core::fmt::Display {}

impl<T: Clone + Ord + core::fmt::Display> EventId for T {}

/// Maximum depth of an `h64` bucket request.
///
/// MSC4521 caps `depth` at 32, so a prefix is at most 32 bits wide. Requests
/// deeper than this MUST be rejected, whoever sends them.
pub const MAX_DEPTH: u8 = 32;

/// Internal bit width of the `h64` trie used to materialize bucket ranges.
pub const H64_TRIE_WIDTH: u8 = 64;

pub use algebraic::{
    gf64_mul, verify_residual, AccumulatedElement, AlgebraicError, ElementHash, EventIdFormat,
    FixedWindowElementHash, RoomAccumulator, RoomEventIdKind, SyndromeSketch,
    MAX_LOCAL_SKETCH_DECODE_CAPACITY, MAX_OVERFLOW_SKETCH_CAPACITY, MAX_SKETCH_CAPACITY,
};
pub use client::{
    should_narrow, BucketExchange, ClientAction, DefaultClientAction, Gf256ClientAction,
    ReconciliationClient, RemoteDigest, MAX_BUCKETS_PER_ROUND, MAX_RECONCILIATION_ROUNDS,
};
pub use gf256::{Gf256, Gf256Decoder, Gf256Sketch};

/// Profile identifier selected by default for full-digest reconciliation.
pub const DEFAULT_PROFILE_ID: &str = gf256::PROFILE_ID;
pub use gf256_exchange::{
    subtract_gf256_payload, Gf256Action, Gf256BucketExchange, Gf256FallbackReason,
};
pub use resident::{
    decode_strata, encode_strata, ResidentKernel, RESIDENT_FORMAT_VERSION, RESIDENT_SERIALIZED_LEN,
    STRATA_COUNT, STRATUM_CAPACITY,
};
pub use server::{
    build_bucket_nodes, build_bucket_sketches, build_gf256_bucket_nodes,
    build_gf256_bucket_sketches, compute_fixed_window_frame_digest, compute_frame_digest,
    FixedWindowPopulation, ForwardGraph, H64Index, IncrementalFixedWindowPopulation, Population,
    ReconciliationContext, SortedPopulation, DEFAULT_BUCKET_CACHE_DEPTH,
};
pub use triage::{
    bucket_prefix, decode_bucket_sketches, decode_gf256_bucket_sketches,
    decode_gf256_bucket_sketches_with_summaries, estimate_strata, recommended_bucket_decode_budget,
    recommended_gf256_bucket_decode_budget, validate_overflow_bucket_requests, BucketDecodeBatch,
    BucketDecodeSuccess, BucketRequest, Gf256BucketDecodeBatch, Gf256BucketDecodeSuccess,
    NodeSummary, StrataEstimate, MAX_BATCH_FACTOR_WORK, MAX_BUCKETED_SKETCH_CAPACITY,
    MAX_OVERFLOW_BUCKET_CAPACITY, MAX_STRATA_FACTOR_WORK, NODE_SUMMARY_LEN,
};
pub use verify::{
    verify_batch, verify_decode, verify_follow_up, verify_gf256_batch, verify_gf256_decode,
    Classified, Gf256Verified,
};

#[allow(clippy::assertions_on_constants)]
const _: () = assert!(MAX_SKETCH_CAPACITY == 32);
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(MAX_OVERFLOW_SKETCH_CAPACITY == 256);
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(resident::STRATA_COUNT == 32);
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(resident::STRATUM_CAPACITY == 8);
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(triage::MAX_BUCKET_SKETCH_CAPACITY == 32);
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(triage::MAX_OVERFLOW_BUCKET_CAPACITY == 256);
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(triage::MAX_BUCKETED_SKETCH_CAPACITY == 4_096);
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(triage::MAX_BATCH_FACTOR_WORK == 36_700_160);
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(triage::MAX_STRATA_FACTOR_WORK == 1_458_176);
