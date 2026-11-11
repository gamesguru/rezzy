//! Run containers for one 65,536-value chunk.
//!
//! A run stores `(start, length - 1)`, so a full 65,536-value range fits in two
//! `u16`s.

use super::{empty_words, Words, WORDS};
use alloc::vec::Vec;

/// A contiguous range of low halves: `len_minus_one + 1` values from `start`.
///
/// Storing `length - 1` lets a full 65 536-value range fit in a `u16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Run {
    pub(super) start: u16,
    pub(super) len_minus_one: u16,
}

impl Run {
    /// Returns the number of values in this run.
    pub(super) fn len(self) -> usize {
        usize::from(self.len_minus_one)
            .checked_add(1)
            .expect("run length fits usize")
    }

    /// Exclusive end, in `0..=65_536`.
    pub(super) fn end(self) -> u32 {
        u32::from(self.start)
            .checked_add(u32::try_from(self.len()).expect("run length fits u32"))
            .expect("run end fits u32")
    }
}

/// Counts runs in sorted, deduplicated values.
pub(super) fn count_runs(values: &[u16]) -> usize {
    let mut runs = usize::from(!values.is_empty());
    for pair in values.windows(2) {
        if pair[0].checked_add(1) != Some(pair[1]) {
            runs = runs.checked_add(1).expect("run count fits usize");
        }
    }
    runs
}

/// Counts runs without allocating: a bit starts a run when it is set and its
/// lower neighbour (including the previous word's top bit) is clear.
pub(super) fn count_runs_words(words: &[u64; WORDS]) -> usize {
    let mut runs = 0usize;
    let mut previous = false;
    for &word in words {
        let starts = word & !(word << 1) & !u64::from(previous);
        runs = runs
            .checked_add(starts.count_ones() as usize)
            .expect("run count fits usize");
        previous = word >> 63 != 0;
    }
    runs
}

/// Builds runs from sorted, deduplicated values.
pub(super) fn runs_from_values(values: &[u16]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for &value in values {
        push_run_value(&mut runs, value);
    }
    runs
}

/// Builds runs from a dense wordset.
pub(super) fn runs_from_words(words: &[u64; WORDS]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (index, &word) in words.iter().enumerate() {
        let mut w = word;
        while w != 0 {
            let base = u16::try_from(index.checked_mul(64).expect("bitmap word base fits usize"))
                .expect("bitmap word base fits u16");
            let offset = u16::try_from(w.trailing_zeros()).expect("bit offset fits u16");
            let value = base.checked_add(offset).expect("bit index fits u16");
            push_run_value(&mut runs, value);
            w &= w.checked_sub(1).expect("nonzero word is positive");
        }
    }
    runs
}

/// Expands runs into a sorted value vector.
pub(super) fn values_from_runs(runs: &[Run]) -> Vec<u16> {
    let total = runs.iter().map(|run| run.len()).sum();
    let mut out = Vec::with_capacity(total);
    for run in runs {
        for offset in 0..run.len() {
            let value = u32::from(run.start)
                .checked_add(u32::try_from(offset).expect("run offset fits u32"))
                .expect("run value fits u32");
            out.push(u16::try_from(value).expect("run value fits u16"));
        }
    }
    out
}

/// Builds a dense wordset from runs.
pub(super) fn words_from_runs(runs: &[Run]) -> Words {
    const WORD_BITS: usize = 64;
    let mut words = empty_words();
    for run in runs {
        let start = usize::from(run.start);
        let end = usize::try_from(run.end()).expect("run end fits usize"); // exclusive
        let mut bit = start;
        while bit < end {
            let word = bit / WORD_BITS;
            let lo = bit % WORD_BITS;
            let word_base = word
                .checked_mul(WORD_BITS)
                .expect("bitmap word base fits usize");
            let word_end = word
                .checked_add(1)
                .and_then(|next| next.checked_mul(WORD_BITS))
                .expect("bitmap word end fits usize");
            let hi = end
                .min(word_end)
                .checked_sub(word_base)
                .expect("word range contains the current bit"); // exclusive, 1..=64
            let width = hi.checked_sub(lo).expect("lo is below hi");
            let mask = if width == WORD_BITS {
                u64::MAX
            } else {
                let lower = 1_u64
                    .checked_shl(u32::try_from(width).expect("width fits u32"))
                    .expect("width is below 64")
                    .checked_sub(1)
                    .expect("width is nonzero");
                lower
                    .checked_shl(u32::try_from(lo).expect("lo fits u32"))
                    .expect("lo is below 64")
            };
            words[word] |= mask;
            bit = word_base.checked_add(hi).expect("next bit fits usize");
        }
    }
    words
}

/// Appends `value`, extending the last run when it is adjacent.
pub(super) fn push_run_value(runs: &mut Vec<Run>, value: u16) {
    match runs.last_mut() {
        Some(run) if run.end() == u32::from(value) => {
            run.len_minus_one = run
                .len_minus_one
                .checked_add(1)
                .expect("run length fits u16");
        }
        _ => runs.push(Run {
            start: value,
            len_minus_one: 0,
        }),
    }
}
