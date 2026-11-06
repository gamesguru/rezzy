//! Container storage and mutation for one 65,536-value chunk.

use super::run::{
    count_runs, count_runs_words, runs_from_values, runs_from_words, values_from_runs,
    words_from_runs, Run,
};
use super::{
    array_to_words, clear_bit, empty_words, popcount, set_bit, test_bit, words_to_array, Words,
    ARRAY_MAX, WORDS,
};
use alloc::vec::Vec;

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Store {
    /// Sorted, deduplicated low halves.
    Array(Vec<u16>),
    /// Bitset over all 65 536 low halves.
    Dense(Words),
    /// Sorted, non-overlapping, non-adjacent ranges.
    Run(Vec<Run>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Array,
    Dense,
    Run,
}

/// Serialized payload bytes of each container, excluding the chunk header.
pub(super) const fn array_bytes(len: usize) -> usize {
    len.checked_mul(2).expect("array size fits usize")
}

const DENSE_BYTES: usize = WORDS * 8;

/// A run container stores a `u16` count followed by `(start, len - 1)` pairs.
pub(super) fn run_bytes(runs: usize) -> usize {
    runs.checked_mul(4)
        .and_then(|n| n.checked_add(2))
        .expect("run size fits usize")
}

/// Picks the smallest canonical store. Ties prefer arrays, then bitsets.
pub(super) fn pick_kind(len: usize, runs: usize) -> Kind {
    let (base, bytes) = if len <= ARRAY_MAX {
        (Kind::Array, array_bytes(len))
    } else {
        (Kind::Dense, DENSE_BYTES)
    };
    if run_bytes(runs) < bytes {
        Kind::Run
    } else {
        base
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Chunk {
    pub(super) key: u16,
    /// Cached cardinality; always equals the number of values in `store`.
    pub(super) len: u32,
    /// Cached number of contiguous ranges; always matches `store`.
    pub(super) runs: u32,
    pub(super) store: Store,
}

fn array_neighbours(values: &[u16], lo: u16) -> (bool, bool) {
    let left = lo > 0
        && values
            .binary_search(&lo.checked_sub(1).expect("lo is nonzero"))
            .is_ok();
    let right = lo < u16::MAX
        && values
            .binary_search(&lo.checked_add(1).expect("lo is below u16::MAX"))
            .is_ok();
    (left, right)
}

pub(super) fn dense_neighbours(words: &[u64; WORDS], lo: u16) -> (bool, bool) {
    let left = lo > 0 && test_bit(words, lo.checked_sub(1).expect("lo is nonzero"));
    let right = lo < u16::MAX && test_bit(words, lo.checked_add(1).expect("lo is below u16::MAX"));
    (left, right)
}

/// Changes in the number of runs when a value is added or removed between the
/// given neighbours: filling both neighbours joins two runs, and removing the
/// only value between two neighbours splits one.
pub(super) const fn neighbour_run_delta(left: bool, right: bool, inserting: bool) -> i32 {
    match (left, right, inserting) {
        (false, false, true) | (true, true, false) => 1,
        (true, true, true) | (false, false, false) => -1,
        _ => 0,
    }
}

pub(super) const fn adjust_count(value: u32, delta: i32) -> u32 {
    value
        .checked_add_signed(delta)
        .expect("cached count stays in range")
}

pub(super) fn zip_words(a: &Words, b: &Words, op: impl Fn(u64, u64) -> u64) -> Words {
    let mut out = empty_words();
    for ((slot, x), y) in out.iter_mut().zip(a.iter()).zip(b.iter()) {
        *slot = op(*x, *y);
    }
    out
}

pub(super) fn insert_array(values: &mut Vec<u16>, lo: u16) -> bool {
    if values.last().is_none_or(|&last| last < lo) {
        values.push(lo);
        return true;
    }
    match values.binary_search(&lo) {
        Ok(_) => false,
        Err(position) => {
            values.insert(position, lo);
            true
        }
    }
}

pub(super) fn insert_run(runs: &mut Vec<Run>, lo: u16) -> Option<u32> {
    let position = runs.partition_point(|run| u32::from(run.start) < u32::from(lo));
    if position != 0 {
        let previous_index = position.checked_sub(1).expect("position is nonzero");
        let previous = runs[previous_index];
        if u32::from(lo) < previous.end() {
            return None;
        }
        if u32::from(lo) == previous.end() {
            runs[previous_index].len_minus_one = runs[previous_index]
                .len_minus_one
                .checked_add(1)
                .expect("run length fits u16");
            if position < runs.len()
                && u32::from(runs[position].start)
                    == u32::from(lo).checked_add(1).expect("lo is below u16::MAX")
            {
                let next = runs.remove(position);
                runs[previous_index].len_minus_one = runs[previous_index]
                    .len_minus_one
                    .checked_add(next.len_minus_one)
                    .and_then(|value| value.checked_add(1))
                    .expect("run length fits u16");
            }
            return Some(u32::try_from(runs.len()).expect("run count fits u32"));
        }
    }
    if position < runs.len() && runs[position].start == lo {
        return None;
    }
    if position < runs.len()
        && u32::from(runs[position].start)
            == u32::from(lo).checked_add(1).expect("lo is below u16::MAX")
    {
        runs[position].start = lo;
        runs[position].len_minus_one = runs[position]
            .len_minus_one
            .checked_add(1)
            .expect("run length fits u16");
    } else {
        runs.insert(
            position,
            Run {
                start: lo,
                len_minus_one: 0,
            },
        );
    }
    Some(u32::try_from(runs.len()).expect("run count fits u32"))
}

pub(super) fn remove_array(values: &mut Vec<u16>, lo: u16) -> Option<i32> {
    let position = values.binary_search(&lo).ok()?;
    let (left, right) = array_neighbours(values, lo);
    values.remove(position);
    Some(neighbour_run_delta(left, right, false))
}

pub(super) fn remove_dense(words: &mut Words, lo: u16) -> Option<i32> {
    let (left, right) = dense_neighbours(words, lo);
    if !clear_bit(words, lo) {
        return None;
    }
    Some(neighbour_run_delta(left, right, false))
}

pub(super) fn remove_run(runs: &mut Vec<Run>, lo: u16) -> Option<u32> {
    let position = runs.partition_point(|run| u32::from(run.start) <= u32::from(lo));
    let index = position.checked_sub(1)?;
    let run = runs[index];
    if u32::from(lo) >= run.end() {
        return None;
    }
    if run.len() == 1 {
        runs.remove(index);
    } else if lo == run.start {
        runs[index].start = lo.checked_add(1).expect("run start fits u16");
        runs[index].len_minus_one = runs[index]
            .len_minus_one
            .checked_sub(1)
            .expect("run has more than one value");
    } else if u32::from(lo).checked_add(1).expect("lo is below u16::MAX") == run.end() {
        runs[index].len_minus_one = runs[index]
            .len_minus_one
            .checked_sub(1)
            .expect("run has more than one value");
    } else {
        let left_len = u32::from(lo)
            .checked_sub(u32::from(run.start))
            .expect("lo is inside the run");
        let right_start = lo.checked_add(1).expect("run start fits u16");
        let right_len = run
            .end()
            .checked_sub(u32::from(lo).checked_add(1).expect("lo is below u16::MAX"))
            .expect("lo is inside the run");
        runs[index].len_minus_one =
            u16::try_from(left_len.checked_sub(1).expect("left run is non-empty"))
                .expect("left run length fits u16");
        runs.insert(
            index.checked_add(1).expect("index fits usize"),
            Run {
                start: right_start,
                len_minus_one: u16::try_from(
                    right_len.checked_sub(1).expect("right run is non-empty"),
                )
                .expect("right run length fits u16"),
            },
        );
    }
    Some(u32::try_from(runs.len()).expect("run count fits u32"))
}

impl Chunk {
    pub(super) fn from_array(key: u16, values: Vec<u16>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let len = u32::try_from(values.len()).expect("bitmap chunk length fits u32");
        let runs = u32::try_from(count_runs(&values)).expect("bitmap run count fits u32");
        let store = match pick_kind(values.len(), runs as usize) {
            Kind::Array => Store::Array(values),
            Kind::Dense => Store::Dense(array_to_words(&values)),
            Kind::Run => Store::Run(runs_from_values(&values)),
        };
        Some(Self {
            key,
            len,
            runs,
            store,
        })
    }

    /// Builds a canonical chunk from a bitset, recounting its cardinality.
    pub(super) fn from_words(key: u16, words: Words) -> Option<Self> {
        let len = popcount(&words);
        Self::from_counted_words(key, words, len)
    }

    /// Builds a canonical chunk from a bitset whose cardinality is `len`.
    pub(super) fn from_counted_words(key: u16, words: Words, len: u32) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let runs = u32::try_from(count_runs_words(&words)).expect("bitmap run count fits u32");
        let store = match pick_kind(
            usize::try_from(len).expect("bitmap length fits usize"),
            runs as usize,
        ) {
            Kind::Array => Store::Array(words_to_array(&words)),
            Kind::Dense => Store::Dense(words),
            Kind::Run => Store::Run(runs_from_words(&words)),
        };
        Some(Self {
            key,
            len,
            runs,
            store,
        })
    }

    /// The representation that `encode` must write. Run containers stay in
    /// memory, but the wire format has no run chunk kind, so runs are
    /// materialized as the smaller of an array and a bitset.
    pub(super) fn encode_store(&self) -> Store {
        let len = usize::try_from(self.len).expect("bitmap length fits usize");
        if len <= ARRAY_MAX {
            Store::Array(self.values())
        } else {
            Store::Dense(self.words())
        }
    }

    /// Promotes a container that has outgrown its representation. There is no
    /// matching demotion: shrinking is canonicalized at `encode`, so a size
    /// boundary cannot thrash on alternating insert/erase.
    fn promote(&mut self) {
        let len = usize::try_from(self.len).expect("bitmap length fits usize");
        let should_promote = match &self.store {
            Store::Array(_) => len > ARRAY_MAX,
            Store::Run(runs) => run_bytes(runs.len()) >= DENSE_BYTES,
            Store::Dense(_) => false,
        };
        if !should_promote {
            return;
        }
        let runs = usize::try_from(self.runs).expect("bitmap run count fits usize");
        self.store = match pick_kind(len, runs) {
            Kind::Run => Store::Run(self.runs_vec()),
            _ => Store::Dense(self.words()),
        };
    }

    pub(super) fn values(&self) -> Vec<u16> {
        match &self.store {
            Store::Array(values) => values.clone(),
            Store::Dense(words) => words_to_array(words),
            Store::Run(runs) => values_from_runs(runs),
        }
    }

    pub(super) fn words(&self) -> Words {
        match &self.store {
            Store::Array(values) => array_to_words(values),
            Store::Dense(words) => words.clone(),
            Store::Run(runs) => words_from_runs(runs),
        }
    }

    pub(super) fn runs_vec(&self) -> Vec<Run> {
        match &self.store {
            Store::Array(values) => runs_from_values(values),
            Store::Dense(words) => runs_from_words(words),
            Store::Run(runs) => runs.clone(),
        }
    }

    pub(super) fn contains(&self, lo: u16) -> bool {
        match &self.store {
            Store::Array(v) => v.binary_search(&lo).is_ok(),
            Store::Dense(w) => test_bit(w, lo),
            Store::Run(runs) => {
                let index = runs.partition_point(|run| u32::from(run.start) <= u32::from(lo));
                index != 0
                    && u32::from(lo) < runs[index.checked_sub(1).expect("index is nonzero")].end()
            }
        }
    }

    pub(super) fn insert(&mut self, lo: u16) -> bool {
        let new_runs = match &mut self.store {
            Store::Array(values) => {
                let (left, right) = array_neighbours(values, lo);
                if !insert_array(values, lo) {
                    return false;
                }
                adjust_count(self.runs, neighbour_run_delta(left, right, true))
            }
            Store::Dense(words) => {
                let (left, right) = dense_neighbours(words, lo);
                if !set_bit(words, lo) {
                    return false;
                }
                adjust_count(self.runs, neighbour_run_delta(left, right, true))
            }
            Store::Run(runs) => match insert_run(runs, lo) {
                Some(count) => count,
                None => return false,
            },
        };
        self.len = adjust_count(self.len, 1);
        self.runs = new_runs;
        self.promote();
        true
    }

    pub(super) fn remove(&mut self, lo: u16) -> bool {
        let new_runs = match &mut self.store {
            Store::Array(values) => match remove_array(values, lo) {
                Some(delta) => adjust_count(self.runs, delta),
                None => return false,
            },
            Store::Dense(words) => match remove_dense(words, lo) {
                Some(delta) => adjust_count(self.runs, delta),
                None => return false,
            },
            Store::Run(runs) => match remove_run(runs, lo) {
                Some(count) => count,
                None => return false,
            },
        };
        self.len = adjust_count(self.len, -1);
        self.runs = new_runs;
        self.promote();
        true
    }

    pub(super) fn is_subset(&self, other: &Self) -> bool {
        if matches!(self.store, Store::Run(_)) || matches!(other.store, Store::Run(_)) {
            let a = self.words();
            let b = other.words();
            return a.iter().zip(b.iter()).all(|(x, y)| x & !y == 0);
        }
        match (&self.store, &other.store) {
            (Store::Array(a), Store::Array(b)) => a.iter().all(|x| b.binary_search(x).is_ok()),
            (Store::Array(a), Store::Dense(b)) => a.iter().all(|&x| test_bit(b, x)),
            (Store::Dense(a), Store::Array(b)) => {
                if self.len as usize > b.len() {
                    return false;
                }
                // Dense chunks are never demoted, so one can hold few enough
                // values to be a subset of an array.
                words_to_array(a).iter().all(|x| b.binary_search(x).is_ok())
            }
            (Store::Dense(a), Store::Dense(b)) => a.iter().zip(b.iter()).all(|(x, y)| x & !y == 0),
            _ => unreachable!("run stores handled above"),
        }
    }

    pub(super) fn is_disjoint(&self, other: &Self) -> bool {
        if matches!(self.store, Store::Run(_)) || matches!(other.store, Store::Run(_)) {
            let a = self.words();
            let b = other.words();
            return a.iter().zip(b.iter()).all(|(x, y)| x & y == 0);
        }
        match (&self.store, &other.store) {
            (Store::Array(a), Store::Array(b)) => {
                let (mut i, mut j) = (0, 0);
                while i < a.len() && j < b.len() {
                    match a[i].cmp(&b[j]) {
                        core::cmp::Ordering::Less => {
                            i = i.checked_add(1).expect("array index fits");
                        }
                        core::cmp::Ordering::Greater => {
                            j = j.checked_add(1).expect("array index fits");
                        }
                        core::cmp::Ordering::Equal => return false,
                    }
                }
                true
            }
            (Store::Array(a), Store::Dense(b)) | (Store::Dense(b), Store::Array(a)) => {
                a.iter().all(|&x| !test_bit(b, x))
            }
            (Store::Dense(a), Store::Dense(b)) => a.iter().zip(b.iter()).all(|(x, y)| x & y == 0),
            _ => unreachable!("run stores handled above"),
        }
    }

    pub(super) fn words_op(&self, other: &Self, op: impl Fn(u64, u64) -> u64) -> Option<Self> {
        let words = zip_words(&self.words(), &other.words(), op);
        Self::from_words(self.key, words)
    }

    pub(super) fn union(&self, other: &Self) -> Self {
        let key = self.key;
        if matches!(self.store, Store::Run(_)) || matches!(other.store, Store::Run(_)) {
            return self
                .words_op(other, |x, y| x | y)
                .expect("union of non-empty chunks");
        }
        let merged = match (&self.store, &other.store) {
            (Store::Array(a), Store::Array(b)) => {
                let capacity = a
                    .len()
                    .checked_add(b.len())
                    .expect("bitmap union size fits");
                let mut out = Vec::with_capacity(capacity);
                let (mut i, mut j) = (0, 0);
                while i < a.len() && j < b.len() {
                    match a[i].cmp(&b[j]) {
                        core::cmp::Ordering::Less => {
                            out.push(a[i]);
                            i = i.checked_add(1).expect("array index fits");
                        }
                        core::cmp::Ordering::Greater => {
                            out.push(b[j]);
                            j = j.checked_add(1).expect("array index fits");
                        }
                        core::cmp::Ordering::Equal => {
                            out.push(a[i]);
                            i = i.checked_add(1).expect("array index fits");
                            j = j.checked_add(1).expect("array index fits");
                        }
                    }
                }
                out.extend_from_slice(&a[i..]);
                out.extend_from_slice(&b[j..]);
                return Self::from_array(key, out).expect("union of non-empty chunks");
            }
            (Store::Dense(a), Store::Dense(b)) => zip_words(a, b, |x, y| x | y),
            (Store::Dense(d), Store::Array(v)) | (Store::Array(v), Store::Dense(d)) => {
                let mut w = d.clone();
                for &x in v {
                    set_bit(&mut w, x);
                }
                w
            }
            _ => unreachable!("run stores handled above"),
        };
        Self::from_words(key, merged).expect("union of non-empty chunks")
    }

    /// Unions `other` into this chunk without replacing dense storage.
    pub(super) fn union_assign(&mut self, other: &Self) {
        debug_assert_eq!(self.key, other.key);
        if matches!(self.store, Store::Run(_)) || matches!(other.store, Store::Run(_)) {
            *self = self.union(other);
            return;
        }

        match (&mut self.store, &other.store) {
            (Store::Dense(a), Store::Dense(b)) => {
                let mut added: u32 = 0;
                for (x, y) in a.iter_mut().zip(b.iter()) {
                    // Only the newly set bits need counting (one popcount per word).
                    added = added
                        .checked_add((y & !*x).count_ones())
                        .expect("bitmap chunk length overflow");
                    *x |= y;
                }
                self.len = self
                    .len
                    .checked_add(added)
                    .expect("bitmap chunk length overflow");
            }
            (Store::Dense(a), Store::Array(b)) => {
                let mut added: u32 = 0;
                for &x in b {
                    added = added
                        .checked_add(u32::from(set_bit(a, x)))
                        .expect("bitmap chunk length overflow");
                }
                self.len = self
                    .len
                    .checked_add(added)
                    .expect("bitmap chunk length overflow");
            }
            (Store::Array(a), Store::Dense(b)) => {
                let mut words = b.clone();
                let mut added: u32 = 0;
                for &x in a.iter() {
                    added = added
                        .checked_add(u32::from(set_bit(&mut words, x)))
                        .expect("bitmap chunk length overflow");
                }
                self.store = Store::Dense(words);
                self.len = other
                    .len
                    .checked_add(added)
                    .expect("bitmap chunk length overflow");
            }
            (Store::Array(_), Store::Array(_)) => {
                *self = self.union(other);
            }
            _ => unreachable!("run stores handled above"),
        }
        self.runs = u32::try_from(count_runs_store(&self.store)).expect("run count fits u32");
    }

    pub(super) fn intersection(&self, other: &Self) -> Option<Self> {
        let key = self.key;
        if matches!(self.store, Store::Run(_)) || matches!(other.store, Store::Run(_)) {
            return self.words_op(other, |x, y| x & y);
        }
        match (&self.store, &other.store) {
            (Store::Array(a), Store::Array(b)) => {
                let mut out = Vec::new();
                let (mut i, mut j) = (0, 0);
                // Branch-free cursor advance: the three-way `match` mispredicts
                // on interleaved random data, which dominates sparse `&`.
                while i < a.len() && j < b.len() {
                    let (lo_a, lo_b) = (a[i], b[j]);
                    if lo_a == lo_b {
                        out.push(lo_a);
                    }
                    i = i
                        .checked_add(usize::from(lo_a <= lo_b))
                        .expect("array index fits");
                    j = j
                        .checked_add(usize::from(lo_b <= lo_a))
                        .expect("array index fits");
                }
                Self::from_array(key, out)
            }
            (Store::Dense(a), Store::Dense(b)) => {
                let w = zip_words(a, b, |x, y| x & y);
                let len = w.iter().map(|word| word.count_ones()).sum();
                Self::from_counted_words(key, w, len)
            }
            (Store::Array(v), Store::Dense(d)) | (Store::Dense(d), Store::Array(v)) => {
                let out = v.iter().copied().filter(|&x| test_bit(d, x)).collect();
                Self::from_array(key, out)
            }
            _ => unreachable!("run stores handled above"),
        }
    }

    pub(super) fn difference(&self, other: &Self) -> Option<Self> {
        let key = self.key;
        if matches!(self.store, Store::Run(_)) || matches!(other.store, Store::Run(_)) {
            return self.words_op(other, |x, y| x & !y);
        }
        match (&self.store, &other.store) {
            (Store::Array(a), Store::Array(b)) => {
                let mut out = Vec::with_capacity(a.len());
                let mut j = 0;
                for &x in a {
                    while j < b.len() && b[j] < x {
                        j = j.checked_add(1).expect("array index fits");
                    }
                    if j >= b.len() || b[j] != x {
                        out.push(x);
                    }
                }
                Self::from_array(key, out)
            }
            (Store::Array(a), Store::Dense(d)) => {
                let out = a.iter().copied().filter(|&x| !test_bit(d, x)).collect();
                Self::from_array(key, out)
            }
            (Store::Dense(a), Store::Array(b)) => {
                let mut w = a.clone();
                for &x in b {
                    w[usize::from(x >> 6)] &= !(1u64 << (x & 63));
                }
                Self::from_words(key, w)
            }
            (Store::Dense(a), Store::Dense(b)) => {
                Self::from_words(key, zip_words(a, b, |x, y| x & !y))
            }
            _ => unreachable!("run stores handled above"),
        }
    }
}

pub(super) fn count_runs_store(store: &Store) -> usize {
    match store {
        Store::Array(values) => count_runs(values),
        Store::Dense(words) => count_runs_words(words),
        Store::Run(runs) => runs.len(),
    }
}
