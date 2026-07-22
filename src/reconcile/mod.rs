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
mod pinsketch;

pub use algebraic::{
    AlgebraicError, BUCKET_COUNT, Bucket, BucketSummary, EventHash, MAX_SKETCH_CAPACITY,
    RoomAccumulator, SyndromeSketch, gf64_mul, verify_residual,
};
