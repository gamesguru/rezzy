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

//! Reversible XOR accumulator over XXH3-128 digests.

use super::xxh3::{Digest, hash_bytes};

/// Incremental room-level XOR accumulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct XorSum {
    sum: Digest,
}

impl XorSum {
    /// Returns the empty accumulator.
    #[must_use]
    #[inline]
    pub const fn new() -> Self {
        Self { sum: 0 }
    }

    /// Returns the current XOR sum.
    #[must_use]
    #[inline]
    pub const fn digest(self) -> Digest {
        self.sum
    }

    /// Applies one digest to the accumulator.
    #[inline]
    pub fn update(&mut self, digest: Digest) {
        self.sum ^= digest;
    }

    /// Inserts a byte slice into the accumulator.
    #[inline]
    pub fn insert_bytes(&mut self, input: &[u8]) {
        self.update(hash_bytes(input));
    }

    /// Inserts a string slice into the accumulator.
    #[inline]
    pub fn insert_str(&mut self, input: &str) {
        self.insert_bytes(input.as_bytes());
    }

    /// Removes a byte slice from the accumulator.
    #[inline]
    pub fn remove_bytes(&mut self, input: &[u8]) {
        self.update(hash_bytes(input));
    }

    /// Removes a string slice from the accumulator.
    #[inline]
    pub fn remove_str(&mut self, input: &str) {
        self.remove_bytes(input.as_bytes());
    }

    /// Replaces one digest with another.
    #[inline]
    pub fn replace(&mut self, old_digest: Digest, new_digest: Digest) {
        self.sum ^= old_digest;
        self.sum ^= new_digest;
    }
}

impl core::ops::BitXorAssign<Digest> for XorSum {
    #[inline]
    fn bitxor_assign(&mut self, rhs: Digest) {
        self.update(rhs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::xxh3::hash_str;

    #[test]
    fn xor_accumulator_roundtrips() {
        let a = hash_str("alpha");
        let b = hash_str("beta");

        let mut acc = XorSum::new();
        acc.update(a);
        acc.update(b);
        acc.update(a);

        assert_eq!(acc.digest(), b);

        acc.replace(b, a);
        assert_eq!(acc.digest(), a);

        acc.remove_str("alpha");
        assert_eq!(acc.digest(), 0);
    }
}
