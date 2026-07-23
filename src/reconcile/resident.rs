// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Incrementally maintained MSC0501 reconciliation state.

use alloc::{vec, vec::Vec};

use super::{
    algebraic::{AlgebraicError, BUCKET_COUNT, EventHash, RoomAccumulator},
    gf64,
};

const MAX_BUCKET_COUNT: u32 = 0x00ff_ffff;
/// Number of estimator strata.
pub const STRATA_COUNT: usize = 32;
/// Extraction capacity maintained in each estimator stratum.
pub const STRATUM_CAPACITY: usize = 8;

/// One resident depth-8 localization bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResidentBucket {
    /// Fault-detection accumulator over full event hashes.
    pub accumulator: u128,
    /// Saturating 24-bit wire count.
    pub count: u32,
    /// Whether the exact count or bucket contents must be rebuilt from storage.
    pub scan_required: bool,
    /// Odd syndrome coordinates `s1` through `s15`.
    pub syndromes: [u64; STRATUM_CAPACITY],
}

/// Per-frame resident reconciliation state over accepted events and rejected tombstones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentKernel {
    accumulator: RoomAccumulator,
    strata: [[u64; STRATUM_CAPACITY]; STRATA_COUNT],
    buckets: Vec<ResidentBucket>,
}

impl Default for ResidentKernel {
    fn default() -> Self {
        Self {
            accumulator: RoomAccumulator::new(),
            strata: [[0; STRATUM_CAPACITY]; STRATA_COUNT],
            buckets: vec![ResidentBucket::default(); BUCKET_COUNT],
        }
    }
}

impl ResidentKernel {
    /// Creates empty resident state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the level-0 room accumulator.
    #[must_use]
    pub const fn accumulator(&self) -> RoomAccumulator {
        self.accumulator
    }

    /// Returns the estimator's odd syndrome coordinates by stratum.
    #[must_use]
    pub const fn strata(&self) -> &[[u64; STRATUM_CAPACITY]; STRATA_COUNT] {
        &self.strata
    }

    /// Returns the depth-8 localization buckets.
    #[must_use]
    pub fn buckets(&self) -> &[ResidentBucket] {
        &self.buckets
    }

    /// Adds an accepted event or rejected-event tombstone.
    ///
    /// # Errors
    /// Returns an error only if the global event count reaches `u64::MAX`.
    pub fn insert(&mut self, hash: EventHash) -> Result<(), AlgebraicError> {
        self.accumulator.insert(hash)?;
        insert_bucket(&mut self.buckets, hash);
        toggle_stratum(&mut self.strata, hash.h64);
        Ok(())
    }

    /// Removes an accepted event or rejected-event tombstone.
    ///
    /// # Errors
    /// Returns an error only if the global event count is already zero.
    pub fn remove(&mut self, hash: EventHash) -> Result<(), AlgebraicError> {
        self.accumulator.remove(hash)?;
        remove_bucket(&mut self.buckets, hash);
        toggle_stratum(&mut self.strata, hash.h64);
        Ok(())
    }
}

fn insert_bucket(buckets: &mut [ResidentBucket], hash: EventHash) {
    let bucket = &mut buckets[(hash.h64 >> 56) as usize];
    if bucket.count == MAX_BUCKET_COUNT {
        bucket.scan_required = true;
    } else {
        bucket.count = bucket.count.saturating_add(1);
    }
    toggle_bucket(bucket, hash);
}

fn remove_bucket(buckets: &mut [ResidentBucket], hash: EventHash) {
    let bucket = &mut buckets[(hash.h64 >> 56) as usize];
    if bucket.count == 0 {
        bucket.scan_required = true;
    } else if !bucket.scan_required {
        bucket.count = bucket.count.saturating_sub(1);
    }
    toggle_bucket(bucket, hash);
}

fn toggle_bucket(bucket: &mut ResidentBucket, hash: EventHash) {
    bucket.accumulator ^= hash.h128;
    let squared = gf64::mul(hash.h64, hash.h64);
    let mut odd_power = hash.h64;
    for syndrome in &mut bucket.syndromes {
        *syndrome ^= odd_power;
        odd_power = gf64::mul(odd_power, squared);
    }
}

fn toggle_stratum(strata: &mut [[u64; STRATUM_CAPACITY]; STRATA_COUNT], value: u64) {
    let trailing_zeros = usize::try_from(value.trailing_zeros()).unwrap_or(STRATA_COUNT - 1);
    let index = trailing_zeros.min(STRATA_COUNT - 1);
    let squared = gf64::mul(value, value);
    let mut odd_power = value;
    for syndrome in &mut strata[index] {
        *syndrome ^= odd_power;
        odd_power = gf64::mul(odd_power, squared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(h128: u128, h64: u64) -> EventHash {
        EventHash { h128, h64 }
    }

    #[test]
    fn updates_all_resident_layers_and_reverses_cleanly() {
        let event = hash(0xfeed, 0x100);
        let mut resident = ResidentKernel::new();

        resident.insert(event).unwrap();
        assert_eq!(resident.accumulator().digest(), event.h128);
        assert_eq!(resident.accumulator().known_event_count(), 1);
        assert_eq!(resident.strata()[8][0], event.h64);
        assert_eq!(resident.buckets()[0].count, 1);

        resident.remove(event).unwrap();
        assert_eq!(resident, ResidentKernel::new());
    }

    #[test]
    fn strata_store_odd_powers_and_cap_large_trailing_zero_counts() {
        let mut resident = ResidentKernel::new();
        let event = hash(1, 1_u64 << 40);
        resident.insert(event).unwrap();

        let stratum = &resident.strata()[STRATA_COUNT - 1];
        let squared = gf64::mul(event.h64, event.h64);
        let mut expected = event.h64;
        for syndrome in stratum {
            assert_eq!(*syndrome, expected);
            expected = gf64::mul(expected, squared);
        }
    }

    #[test]
    fn saturated_bucket_marks_scan_required_without_failing_insert() {
        let event = hash(0xfeed, 1);
        let mut resident = ResidentKernel::new();
        resident.buckets[0].count = MAX_BUCKET_COUNT;

        resident.insert(event).unwrap();

        let bucket = &resident.buckets()[0];
        assert_eq!(bucket.count, MAX_BUCKET_COUNT);
        assert!(bucket.scan_required);
        assert_eq!(bucket.accumulator, event.h128);
        assert_eq!(bucket.syndromes[0], event.h64);
    }
}
