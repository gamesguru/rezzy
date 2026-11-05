// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Systematic Reed-Solomon erasure coding over [`Gf8`].
//!
//! A [`ReedSolomon`] configuration has `k` data shards and `p` parity shards,
//! for `n = k + p <= 255` shards total. Encoding produces the parity shards;
//! decoding reconstructs the original `k` data shards from any `k` surviving
//! shards.
//!
//! This is an **erasure** code. The caller must say which shards are missing
//! by passing `None`. It does not detect or correct silently corrupted shards;
//! use a checksum first if shard damage must be distinguished from erasure.
//!
//! The systematic generator matrix is `[I | C]`, where `C` is a Cauchy matrix
//! over the nonzero elements of GF(2^8). It is MDS, so every subset of `k`
//! rows is invertible.
//!
//! # Performance
//!
//! Construction builds a 256-byte multiplication table per parity/data
//! coefficient. Encoding reuses those tables and is `O(p · k · len)`. Erasure
//! decoding adds a `k × k` matrix inversion and is
//! `O(k³ + k² · (len + 256))`. The implementation is intended
//! for modest shard counts and shard lengths; very large workloads may want a
//! dedicated SIMD codec.

use crate::gf8::Gf8;
use alloc::{vec, vec::Vec};

/// A systematic Reed-Solomon configuration over GF(2^8).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReedSolomon {
    data_shards: usize,
    parity_shards: usize,
    /// Cauchy parity coefficients in row-major `parity_shards × data_shards`
    /// order.
    parity_matrix: Vec<Gf8>,
    /// Byte lookup tables for the fixed encoding coefficients, in the same
    /// row-major order as `parity_matrix`.
    parity_tables: Vec<[u8; 256]>,
}

/// Invalid Reed-Solomon configuration, input, or shard set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReedSolomonError {
    /// Both shard counts must be nonzero and their sum must not exceed 255.
    InvalidShardCount,
    /// The number of shards does not match the configuration.
    ShardCountMismatch {
        /// Number of shards the configuration expects.
        expected: usize,
        /// Number of shards the caller supplied.
        actual: usize,
    },
    /// The supplied shards did not all have the same length.
    UnequalShardLengths,
    /// Fewer than `data_shards` shards survived.
    NotEnoughShards {
        /// Number of shards required to decode.
        required: usize,
        /// Number of shards available.
        available: usize,
    },
    /// The selected generator rows were not invertible.
    SingularMatrix,
}

impl ReedSolomon {
    /// Builds a codec with `data_shards` data shards and `parity_shards`
    /// parity shards.
    ///
    /// # Errors
    /// Returns [`ReedSolomonError::InvalidShardCount`] when either count is
    /// zero or `data_shards + parity_shards > 255`.
    ///
    /// # Panics
    /// Never: the count check bounds every shard index below 255.
    pub fn new(data_shards: usize, parity_shards: usize) -> Result<Self, ReedSolomonError> {
        let total = data_shards
            .checked_add(parity_shards)
            .ok_or(ReedSolomonError::InvalidShardCount)?;
        if data_shards == 0 || parity_shards == 0 || total > 255 {
            return Err(ReedSolomonError::InvalidShardCount);
        }

        let capacity = parity_shards
            .checked_mul(data_shards)
            .ok_or(ReedSolomonError::InvalidShardCount)?;
        let mut parity_matrix = Vec::with_capacity(capacity);
        let mut parity_tables = Vec::with_capacity(capacity);
        for parity_index in 0..parity_shards {
            // Data columns use α^j; parity rows use α^(k+i). The two ranges are
            // disjoint because k + p <= 255, so every Cauchy denominator is
            // nonzero.
            // `total <= 255` guarantees both conversions fit in `u32`.
            let left = Gf8::exp(
                u32::try_from(data_shards.saturating_add(parity_index))
                    .expect("shard index below 255"),
            );
            for data_index in 0..data_shards {
                let right = Gf8::exp(u32::try_from(data_index).expect("shard index below 255"));
                let coefficient = left
                    .plus(right)
                    .inv()
                    .ok_or(ReedSolomonError::SingularMatrix)?;
                parity_matrix.push(coefficient);
                parity_tables.push(coefficient.mul_table());
            }
        }

        Ok(Self {
            data_shards,
            parity_shards,
            parity_matrix,
            parity_tables,
        })
    }

    /// Number of data shards.
    #[must_use]
    pub const fn data_shards(&self) -> usize {
        self.data_shards
    }

    /// Number of parity shards.
    #[must_use]
    pub const fn parity_shards(&self) -> usize {
        self.parity_shards
    }

    /// Total number of shards (`data_shards + parity_shards`).
    #[must_use]
    pub const fn shard_count(&self) -> usize {
        self.data_shards.saturating_add(self.parity_shards)
    }

    /// Encodes `data`, returning only the parity shards.
    ///
    /// # Errors
    /// Returns [`ReedSolomonError::ShardCountMismatch`] when `data` does not
    /// contain exactly [`Self::data_shards`] entries, or
    /// [`ReedSolomonError::UnequalShardLengths`] when the data shards differ in
    /// length.
    pub fn encode<S: AsRef<[u8]>>(&self, data: &[S]) -> Result<Vec<Vec<u8>>, ReedSolomonError> {
        if data.len() != self.data_shards {
            return Err(ReedSolomonError::ShardCountMismatch {
                expected: self.data_shards,
                actual: data.len(),
            });
        }
        let length = data.first().map_or(0, |shard| shard.as_ref().len());
        if data.iter().any(|shard| shard.as_ref().len() != length) {
            return Err(ReedSolomonError::UnequalShardLengths);
        }

        let mut parity = vec![vec![0_u8; length]; self.parity_shards];
        for (parity_index, output) in parity.iter_mut().enumerate() {
            for (data_index, input) in data.iter().enumerate() {
                let table = &self.parity_tables[parity_index
                    .saturating_mul(self.data_shards)
                    .saturating_add(data_index)];
                for (out, &value) in output.iter_mut().zip(input.as_ref()) {
                    *out ^= table[usize::from(value)];
                }
            }
        }
        Ok(parity)
    }

    /// Encodes `data` and returns all shards in `[data..., parity...]` order.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::encode`].
    pub fn encode_systematic<S: AsRef<[u8]>>(
        &self,
        data: &[S],
    ) -> Result<Vec<Vec<u8>>, ReedSolomonError> {
        let parity = self.encode(data)?;
        let mut shards = Vec::with_capacity(self.shard_count());
        shards.extend(data.iter().map(|shard| shard.as_ref().to_vec()));
        shards.extend(parity);
        Ok(shards)
    }

    /// Reconstructs the `k` data shards from `n` shard slots.
    ///
    /// `shards` must contain exactly [`Self::shard_count`] entries in index
    /// order, with `None` for each missing shard. Any `k` surviving shards are
    /// sufficient. The returned vector contains only the data shards, in their
    /// original order.
    ///
    /// # Errors
    /// Returns [`ReedSolomonError::ShardCountMismatch`] when `shards` has the
    /// wrong number of slots, [`ReedSolomonError::NotEnoughShards`] when fewer
    /// than `k` survive, [`ReedSolomonError::UnequalShardLengths`] when the
    /// survivors differ in length, or [`ReedSolomonError::SingularMatrix`] if
    /// the selected generator rows are singular (this should not happen for a
    /// valid configuration).
    pub fn decode(&self, shards: &[Option<&[u8]>]) -> Result<Vec<Vec<u8>>, ReedSolomonError> {
        let total = self.shard_count();
        if shards.len() != total {
            return Err(ReedSolomonError::ShardCountMismatch {
                expected: total,
                actual: shards.len(),
            });
        }

        let available: Vec<(usize, &[u8])> = shards
            .iter()
            .enumerate()
            .filter_map(|(index, shard)| shard.map(|shard| (index, shard)))
            .collect();
        if available.len() < self.data_shards {
            return Err(ReedSolomonError::NotEnoughShards {
                required: self.data_shards,
                available: available.len(),
            });
        }
        let length = available[0].1.len();
        if available.iter().any(|(_, shard)| shard.len() != length) {
            return Err(ReedSolomonError::UnequalShardLengths);
        }

        // The common case: every data shard is present, so no matrix work is
        // needed even if parity shards are missing.
        if shards[..self.data_shards].iter().all(Option::is_some) {
            return Ok(shards[..self.data_shards]
                .iter()
                .filter_map(|shard| shard.map(<[u8]>::to_vec))
                .collect());
        }

        let selected = &available[..self.data_shards];
        let matrix = self.selected_matrix(selected.iter().map(|(index, _)| *index));
        let inverse = invert(matrix).ok_or(ReedSolomonError::SingularMatrix)?;

        let mut data = vec![vec![0_u8; length]; self.data_shards];
        for (output, coefficients) in data.iter_mut().zip(&inverse) {
            for (coefficient, (_, shard)) in coefficients.iter().zip(selected) {
                if coefficient.is_zero() {
                    continue;
                }
                let table = coefficient.mul_table();
                for (out, &value) in output.iter_mut().zip(*shard) {
                    *out ^= table[usize::from(value)];
                }
            }
        }
        Ok(data)
    }

    fn coefficient(&self, row: usize, column: usize) -> Gf8 {
        if row < self.data_shards {
            if row == column {
                Gf8::ONE
            } else {
                Gf8::ZERO
            }
        } else {
            self.parity_matrix[row
                .saturating_sub(self.data_shards)
                .saturating_mul(self.data_shards)
                .saturating_add(column)]
        }
    }

    fn selected_matrix(&self, rows: impl Iterator<Item = usize>) -> Vec<Vec<Gf8>> {
        rows.map(|row| {
            (0..self.data_shards)
                .map(|column| self.coefficient(row, column))
                .collect()
        })
        .collect()
    }
}

/// Inverts a square matrix over GF(2^8) with Gauss-Jordan elimination.
fn invert(matrix: Vec<Vec<Gf8>>) -> Option<Vec<Vec<Gf8>>> {
    let size = matrix.len();
    let width = size.checked_mul(2)?;
    let mut augmented = Vec::with_capacity(size);
    for (row_index, coefficients) in matrix.into_iter().enumerate() {
        let mut row = vec![Gf8::ZERO; width];
        row[..size].copy_from_slice(&coefficients);
        row[size.saturating_add(row_index)] = Gf8::ONE;
        augmented.push(row);
    }

    for pivot_column in 0..size {
        let pivot_row =
            (pivot_column..size).find(|&row| !augmented[row][pivot_column].is_zero())?;
        augmented.swap(pivot_column, pivot_row);

        let pivot_inverse = augmented[pivot_column][pivot_column].inv()?;
        for value in &mut augmented[pivot_column] {
            *value = value.times(pivot_inverse);
        }

        for row_index in 0..size {
            if row_index == pivot_column {
                continue;
            }
            let factor = augmented[row_index][pivot_column];
            if factor.is_zero() {
                continue;
            }
            let pivot = core::mem::take(&mut augmented[pivot_column]);
            for (target, source) in augmented[row_index].iter_mut().zip(&pivot) {
                *target = target.plus(source.times(factor));
            }
            augmented[pivot_column] = pivot;
        }
    }

    Some(
        augmented
            .into_iter()
            .map(|row| row[size..].to_vec())
            .collect(),
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn sample_data(shards: usize, length: usize) -> Vec<Vec<u8>> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..shards)
            .map(|_| {
                (0..length)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state.to_le_bytes()[0]
                    })
                    .collect()
            })
            .collect()
    }

    fn decode_with_erasures(
        codec: &ReedSolomon,
        shards: &[Vec<u8>],
        mask: u16,
    ) -> Result<Vec<Vec<u8>>, ReedSolomonError> {
        let slots: Vec<Option<&[u8]>> = shards
            .iter()
            .enumerate()
            .map(|(index, shard)| {
                if mask & (1_u16 << index) == 0 {
                    Some(shard.as_slice())
                } else {
                    None
                }
            })
            .collect();
        codec.decode(&slots)
    }

    #[test]
    fn rejects_invalid_shard_counts() {
        assert_eq!(
            ReedSolomon::new(0, 1),
            Err(ReedSolomonError::InvalidShardCount)
        );
        assert_eq!(
            ReedSolomon::new(1, 0),
            Err(ReedSolomonError::InvalidShardCount)
        );
        assert_eq!(
            ReedSolomon::new(255, 1),
            Err(ReedSolomonError::InvalidShardCount)
        );
        assert!(ReedSolomon::new(200, 55).is_ok());
    }

    #[test]
    fn round_trips_every_combination_up_to_the_parity_width() {
        let codec = ReedSolomon::new(4, 2).expect("valid configuration");
        let data = sample_data(4, 33);
        let shards = codec.encode_systematic(&data).expect("encoding succeeds");
        assert_eq!(shards.len(), 6);

        for mask in 0_u16..(1_u16 << 6) {
            if mask.count_ones() > 2 {
                continue;
            }
            let decoded = decode_with_erasures(&codec, &shards, mask).expect("decoding succeeds");
            assert_eq!(decoded, data, "erasure mask {mask:#08b}");
        }
    }

    #[test]
    fn decodes_with_only_data_shards_or_only_parity_shards() {
        let codec = ReedSolomon::new(3, 3).expect("valid configuration");
        let data = sample_data(3, 7);
        let shards = codec.encode_systematic(&data).expect("encoding succeeds");

        let data_only = decode_with_erasures(&codec, &shards, 0b111_000).expect("data decode");
        assert_eq!(data_only, data);
        let parity_only = decode_with_erasures(&codec, &shards, 0b000_111).expect("parity decode");
        assert_eq!(parity_only, data);
    }

    #[test]
    fn reports_insufficient_shards() {
        let codec = ReedSolomon::new(4, 2).expect("valid configuration");
        let data = sample_data(4, 1);
        let shards = codec.encode_systematic(&data).expect("encoding succeeds");
        assert_eq!(
            decode_with_erasures(&codec, &shards, 0b111_000),
            Err(ReedSolomonError::NotEnoughShards {
                required: 4,
                available: 3,
            })
        );
    }

    #[test]
    fn rejects_mismatched_lengths_and_counts() {
        let codec = ReedSolomon::new(2, 1).expect("valid configuration");
        assert_eq!(
            codec.encode(&[vec![0_u8; 4], vec![0_u8; 5]]),
            Err(ReedSolomonError::UnequalShardLengths)
        );
        assert_eq!(
            codec.encode(&[vec![0_u8; 4]]),
            Err(ReedSolomonError::ShardCountMismatch {
                expected: 2,
                actual: 1,
            })
        );
        assert_eq!(
            codec.decode(&[Some(&[0_u8; 4]), None]),
            Err(ReedSolomonError::ShardCountMismatch {
                expected: 3,
                actual: 2,
            })
        );
        assert_eq!(
            codec.decode(&[Some(&[0_u8; 4]), Some(&[0_u8; 5]), None]),
            Err(ReedSolomonError::UnequalShardLengths)
        );
    }

    #[test]
    fn maximum_width_codec_recovers_maximum_erasures() {
        let codec = ReedSolomon::new(200, 55).expect("valid configuration");
        let data = sample_data(200, 16);
        let shards = codec.encode_systematic(&data).expect("encoding succeeds");
        assert_eq!(shards.len(), 255);
        // Erase regularly spaced shards, then all-parity-for-data.
        for (stride, expected_erasures) in [(3_usize, 55), (4, 55), (5, 51)] {
            let slots: Vec<Option<&[u8]>> = shards
                .iter()
                .enumerate()
                .map(|(i, s)| (i % stride != 0 || i / stride >= 55).then_some(s.as_slice()))
                .collect();
            assert_eq!(
                slots.iter().filter(|s| s.is_none()).count(),
                expected_erasures
            );
            assert_eq!(codec.decode(&slots).expect("decodes"), data);
        }
        let slots: Vec<Option<&[u8]>> = shards
            .iter()
            .enumerate()
            .map(|(i, s)| (i >= 55).then_some(s.as_slice()))
            .collect();
        assert_eq!(codec.decode(&slots).expect("decodes"), data);
    }

    #[test]
    fn missing_only_parity_uses_the_fast_path() {
        let codec = ReedSolomon::new(3, 2).expect("valid configuration");
        let data = sample_data(3, 5);
        let shards = codec.encode_systematic(&data).expect("encoding succeeds");
        assert_eq!(decode_with_erasures(&codec, &shards, 0b11_000), Ok(data));
    }

    #[test]
    fn single_byte_shards_round_trip() {
        let codec = ReedSolomon::new(3, 2).expect("valid configuration");
        let data = sample_data(3, 1);
        let shards = codec.encode_systematic(&data).expect("encoding succeeds");
        assert_eq!(decode_with_erasures(&codec, &shards, 0b00_101), Ok(data));
    }

    #[test]
    fn empty_shards_round_trip() {
        let codec = ReedSolomon::new(2, 2).expect("valid configuration");
        let data = vec![Vec::new(), Vec::new()];
        let shards = codec.encode_systematic(&data).expect("encoding succeeds");
        assert_eq!(decode_with_erasures(&codec, &shards, 0b1100), Ok(data));
    }
}
