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

//! The `algebraic_v1` primitives from MSC0501.

use alloc::{string::String, vec, vec::Vec};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest as _, Sha256};

/// The number of localization buckets in the `algebraic_v1` profile.
pub const BUCKET_COUNT: usize = 256;
/// Maximum extraction capacity accepted by the profile.
pub const MAX_SKETCH_CAPACITY: usize = 50_000;

/// An invalid event identifier, wire digest, or sketch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlgebraicError {
    InvalidEventId,
    InvalidBase64,
    InvalidDigestLength,
    InvalidSketchCapacity,
    InvalidSketchLength,
    DecodeFailure,
    ZeroShortIdentifier,
    CountOverflow,
    CountUnderflow,
}

/// The two truncations of an event's SHA-256 identifier used by MSC0501.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventHash {
    /// First 128 bits, interpreted in network byte order.
    pub h128: u128,
    /// First 64 bits, interpreted in network byte order.
    pub h64: u64,
}

impl EventHash {
    /// Derives hashes from a Matrix event ID.
    ///
    /// For hash-derived room versions, the part after `$` is decoded directly.
    /// Legacy event IDs are SHA-256 hashed as required by the MSC.
    ///
    /// # Errors
    /// Returns an error when the ID has no `$` sigil, contains invalid base64,
    /// or its decoded hash is shorter than 128 bits.
    pub fn from_event_id(event_id: &str, hash_derived: bool) -> Result<Self, AlgebraicError> {
        let encoded = event_id
            .strip_prefix('$')
            .ok_or(AlgebraicError::InvalidEventId)?;
        let bytes = if hash_derived {
            let encoded = encoded.split_once(':').map_or(encoded, |(hash, _)| hash);
            URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| AlgebraicError::InvalidBase64)?
        } else {
            Sha256::digest(event_id.as_bytes()).to_vec()
        };
        if bytes.len() < 16 {
            return Err(AlgebraicError::InvalidEventId);
        }
        let mut wide = [0; 16];
        wide.copy_from_slice(&bytes[..16]);
        let mut short = [0; 8];
        short.copy_from_slice(&bytes[..8]);
        Ok(Self {
            h128: u128::from_be_bytes(wide),
            h64: u64::from_be_bytes(short),
        })
    }
}

/// Incrementally maintained level-0 room digest and exact known-event count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoomAccumulator {
    digest: u128,
    count: u64,
}

impl RoomAccumulator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            digest: 0,
            count: 0,
        }
    }
    #[must_use]
    pub const fn digest(self) -> u128 {
        self.digest
    }
    #[must_use]
    pub const fn known_event_count(self) -> u64 {
        self.count
    }

    /// Adds a known event.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::CountOverflow`] at `u64::MAX` events.
    pub fn insert(&mut self, hash: EventHash) -> Result<(), AlgebraicError> {
        self.count = self
            .count
            .checked_add(1)
            .ok_or(AlgebraicError::CountOverflow)?;
        self.digest ^= hash.h128;
        Ok(())
    }

    /// Removes a known event.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::CountUnderflow`] when the accumulator is empty.
    pub fn remove(&mut self, hash: EventHash) -> Result<(), AlgebraicError> {
        self.count = self
            .count
            .checked_sub(1)
            .ok_or(AlgebraicError::CountUnderflow)?;
        self.digest ^= hash.h128;
        Ok(())
    }

    #[must_use]
    pub fn encode_digest(self) -> String {
        URL_SAFE_NO_PAD.encode(self.digest.to_be_bytes())
    }

    /// Decodes a level-0 digest.
    ///
    /// # Errors
    /// Returns an error for invalid base64 or any decoded length other than 16 bytes.
    pub fn decode_digest(encoded: &str) -> Result<u128, AlgebraicError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| AlgebraicError::InvalidBase64)?;
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| AlgebraicError::InvalidDigestLength)?;
        Ok(u128::from_be_bytes(bytes))
    }

    #[must_use]
    pub const fn residual(self, other: Self) -> u128 {
        self.digest ^ other.digest
    }

    /// Computes the unquoted opaque value for the MSC0501 HTTP `ETag`.
    ///
    /// # Panics
    /// Panics only if `serde_json` fails to serialize a sequence of strings.
    #[must_use]
    pub fn etag<'a>(self, extremity_event_ids: impl IntoIterator<Item = &'a str>) -> String {
        let mut extremities: Vec<&str> = extremity_event_ids.into_iter().collect();
        extremities.sort_unstable();
        let canonical = serde_json::to_vec(&extremities).expect("string arrays are serializable");
        let frontier_hash = Sha256::digest(canonical);
        let mut etag = Vec::with_capacity(24);
        etag.extend_from_slice(&self.digest.to_be_bytes());
        etag.extend_from_slice(&frontier_hash[..8]);
        URL_SAFE_NO_PAD.encode(etag)
    }
}

/// Checks decoded difference identifiers against the 128-bit integrity residual.
#[must_use]
pub fn verify_residual(
    expected_residual: u128,
    hashes: impl IntoIterator<Item = EventHash>,
) -> bool {
    hashes
        .into_iter()
        .fold(0, |residual, hash| residual ^ hash.h128)
        == expected_residual
}

/// Carry-less multiplication in GF(2^64), reduced by `x^64+x^4+x^3+x+1`.
#[must_use]
pub fn gf64_mul(mut a: u64, mut b: u64) -> u64 {
    let mut product = 0;
    for _ in 0..64 {
        if b & 1 != 0 {
            product ^= a;
        }
        b >>= 1;
        let carry = a >> 63;
        a <<= 1;
        if carry != 0 {
            a ^= 0x1b;
        }
    }
    product
}

/// Odd syndrome coordinates `s1, s3, ... s(2k-1)` over GF(2^64).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyndromeSketch {
    coordinates: Vec<u64>,
}

impl SyndromeSketch {
    /// Allocates an empty sketch with the requested extraction capacity.
    ///
    /// # Errors
    /// Returns an error for zero capacity or capacity above the profile maximum.
    pub fn new(capacity: usize) -> Result<Self, AlgebraicError> {
        if capacity == 0 || capacity > MAX_SKETCH_CAPACITY {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
        Ok(Self {
            coordinates: vec![0; capacity],
        })
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.coordinates.len()
    }
    #[must_use]
    pub fn coordinates(&self) -> &[u64] {
        &self.coordinates
    }

    /// Inserts or removes a short identifier. Both operations are XOR in characteristic two.
    /// # Errors
    /// Returns an error because zero is not representable by a `PinSketch`.
    pub fn toggle(&mut self, value: u64) -> Result<(), AlgebraicError> {
        if value == 0 {
            return Err(AlgebraicError::ZeroShortIdentifier);
        }
        let squared = gf64_mul(value, value);
        let mut odd_power = value;
        for coordinate in &mut self.coordinates {
            *coordinate ^= odd_power;
            odd_power = gf64_mul(odd_power, squared);
        }
        Ok(())
    }

    /// Decodes up to `max_elements` from this residual sketch.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::DecodeFailure`] when the residual exceeds the
    /// bound, is malformed, or does not factor into distinct field elements.
    pub fn decode_elements(&self, max_elements: usize) -> Result<Vec<u64>, AlgebraicError> {
        if max_elements > self.capacity() {
            return Err(AlgebraicError::InvalidSketchCapacity);
        }
        let decoded = super::pinsketch::decode(&self.coordinates, max_elements)
            .ok_or(AlgebraicError::DecodeFailure)?;
        let mut check = Self::new(self.capacity())?;
        for element in &decoded {
            check.toggle(*element)?;
        }
        (check == *self)
            .then_some(decoded)
            .ok_or(AlgebraicError::DecodeFailure)
    }

    /// XOR-subtracts another sketch.
    ///
    /// # Errors
    /// Returns an error when sketch capacities differ.
    pub fn subtract(&self, other: &Self) -> Result<Self, AlgebraicError> {
        if self.capacity() != other.capacity() {
            return Err(AlgebraicError::InvalidSketchLength);
        }
        Ok(Self {
            coordinates: self
                .coordinates
                .iter()
                .zip(&other.coordinates)
                .map(|(a, b)| a ^ b)
                .collect(),
        })
    }

    #[must_use]
    pub fn encode(&self) -> String {
        let byte_len = self.coordinates.len().checked_mul(8).unwrap_or(0);
        let mut bytes = Vec::with_capacity(byte_len);
        for coordinate in &self.coordinates {
            bytes.extend_from_slice(&coordinate.to_be_bytes());
        }
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Decodes a sketch with an externally negotiated capacity.
    ///
    /// # Errors
    /// Returns an error for invalid capacity, base64, or encoded byte length.
    pub fn decode(capacity: usize, encoded: &str) -> Result<Self, AlgebraicError> {
        let sketch = Self::new(capacity)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| AlgebraicError::InvalidBase64)?;
        let expected_len = sketch
            .coordinates
            .len()
            .checked_mul(8)
            .ok_or(AlgebraicError::InvalidSketchLength)?;
        if bytes.len() != expected_len {
            return Err(AlgebraicError::InvalidSketchLength);
        }
        let coordinates = bytes
            .chunks_exact(8)
            .map(|chunk| {
                let mut value = [0; 8];
                value.copy_from_slice(chunk);
                u64::from_be_bytes(value)
            })
            .collect();
        Ok(Self { coordinates })
    }
}

/// One resident localization bucket.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bucket {
    pub accumulator: u128,
    pub count: u32,
    /// Resident fast-path coordinates s1 through s15 (odd powers only).
    pub syndromes: [u64; 8],
}

/// The 256-bucket resident structure described by MSC0501.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketSummary {
    buckets: Vec<Bucket>,
}

impl Default for BucketSummary {
    fn default() -> Self {
        Self {
            buckets: vec![Bucket::default(); BUCKET_COUNT],
        }
    }
}

impl BucketSummary {
    #[must_use]
    pub fn buckets(&self) -> &[Bucket] {
        &self.buckets
    }

    /// Adds an event to its leading-byte bucket.
    ///
    /// # Errors
    /// Returns an error when the bucket's 24-bit wire count is exhausted.
    pub fn insert(&mut self, hash: EventHash) -> Result<(), AlgebraicError> {
        let bucket = &mut self.buckets[(hash.h64 >> 56) as usize];
        if bucket.count == 0x00ff_ffff {
            return Err(AlgebraicError::CountOverflow);
        }
        bucket.count = bucket
            .count
            .checked_add(1)
            .ok_or(AlgebraicError::CountOverflow)?;
        toggle_bucket(bucket, hash);
        Ok(())
    }

    /// Removes an event from its leading-byte bucket.
    ///
    /// # Errors
    /// Returns an error when the selected bucket is empty.
    pub fn remove(&mut self, hash: EventHash) -> Result<(), AlgebraicError> {
        let bucket = &mut self.buckets[(hash.h64 >> 56) as usize];
        bucket.count = bucket
            .count
            .checked_sub(1)
            .ok_or(AlgebraicError::CountUnderflow)?;
        toggle_bucket(bucket, hash);
        Ok(())
    }
}

fn toggle_bucket(bucket: &mut Bucket, hash: EventHash) {
    bucket.accumulator ^= hash.h128;
    let squared = gf64_mul(hash.h64, hash.h64);
    let mut odd_power = hash.h64;
    for syndrome in &mut bucket.syndromes {
        *syndrome ^= odd_power;
        odd_power = gf64_mul(odd_power, squared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> EventHash {
        EventHash {
            h128: u128::from(seed) << 120,
            h64: u64::from(seed) << 56,
        }
    }

    #[test]
    fn accumulator_wire_form_is_exactly_sixteen_bytes() {
        let mut accumulator = RoomAccumulator::new();
        accumulator.insert(hash(7)).unwrap();
        assert_eq!(
            RoomAccumulator::decode_digest(&accumulator.encode_digest()).unwrap(),
            accumulator.digest()
        );
        accumulator.remove(hash(7)).unwrap();
        assert_eq!(accumulator, RoomAccumulator::new());
    }

    #[test]
    fn sketch_subtraction_recovers_toggled_syndromes() {
        let mut left = SyndromeSketch::new(4).unwrap();
        let mut right = SyndromeSketch::new(4).unwrap();
        left.toggle(2).unwrap();
        left.toggle(3).unwrap();
        right.toggle(2).unwrap();
        let residual = left.subtract(&right).unwrap();
        let mut expected = SyndromeSketch::new(4).unwrap();
        expected.toggle(3).unwrap();
        assert_eq!(residual, expected);
        assert_eq!(
            SyndromeSketch::decode(4, &residual.encode()).unwrap(),
            residual
        );
    }

    #[test]
    fn bucket_updates_are_reversible() {
        let mut summary = BucketSummary::default();
        summary.insert(hash(42)).unwrap();
        assert_eq!(summary.buckets()[42].count, 1);
        summary.remove(hash(42)).unwrap();
        assert_eq!(summary, BucketSummary::default());
    }

    #[test]
    fn etag_is_independent_of_extremity_order() {
        let accumulator = RoomAccumulator {
            digest: 42,
            count: 2,
        };
        assert_eq!(
            accumulator.etag(["$b", "$a"]),
            accumulator.etag(["$a", "$b"]),
        );
        assert_ne!(accumulator.etag(["$a"]), accumulator.etag(["$b"]));
    }

    #[test]
    fn counters_reject_underflow_and_profile_overflow() {
        let event = hash(1);
        assert_eq!(
            RoomAccumulator::new().remove(event),
            Err(AlgebraicError::CountUnderflow)
        );

        let mut accumulator = RoomAccumulator {
            digest: 0,
            count: u64::MAX,
        };
        assert_eq!(
            accumulator.insert(event),
            Err(AlgebraicError::CountOverflow)
        );

        let mut summary = BucketSummary::default();
        summary.buckets[1].count = 0x00ff_ffff;
        assert_eq!(summary.insert(event), Err(AlgebraicError::CountOverflow));
        assert_eq!(
            BucketSummary::default().remove(event),
            Err(AlgebraicError::CountUnderflow)
        );
    }
}
