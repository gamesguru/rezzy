// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Homomorphic reconciliation helpers.

pub mod algebraic;
pub mod client;
pub mod gf64;
pub mod gf64_simd;
mod pinsketch;
pub mod resident;
pub mod server;
pub mod triage;

/// Internal bit width of the `h64` trie used to materialize bucket ranges.
///
/// This is not the protocol request-depth cap; request validation enforces
/// depth <= 32 in `triage::validate_bucket_requests`.
pub const H64_TRIE_WIDTH: u8 = 64;

pub use algebraic::{
    gf64_mul, verify_residual, AlgebraicError, ElementHash, EventIdFormat, RoomAccumulator,
    SyndromeSketch, MAX_LOCAL_SKETCH_DECODE_CAPACITY, MAX_SKETCH_CAPACITY,
};
pub use client::{
    BucketExchange, ClientAction, ReconciliationClient, RemoteDigest, MAX_BUCKETS_PER_ROUND,
};
pub use resident::{ResidentKernel, STRATA_COUNT, STRATUM_CAPACITY};
pub use server::{
    build_bucket_sketches, compute_frame_digest, ForwardGraph, H64Index, ReconciliationContext,
};
pub use triage::{
    decode_bucket_sketches, estimate_delta, BucketDecodeBatch, BucketDecodeSuccess, BucketRequest,
    MAX_BUCKETED_SKETCH_CAPACITY,
};

const _: [(); 32] = [(); MAX_SKETCH_CAPACITY];
const _: [(); 32] = [(); resident::STRATA_COUNT];
const _: [(); 8] = [(); resident::STRATUM_CAPACITY];
const _: [(); 32] = [(); triage::MAX_BUCKET_SKETCH_CAPACITY];
const _: [(); 4_096] = [(); triage::MAX_BUCKETED_SKETCH_CAPACITY];
