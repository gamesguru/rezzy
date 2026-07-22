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

//! XXH3-128 helpers for reconciliation digests.

/// A 128-bit XXH3 digest.
pub type Digest = u128;

/// Computes the XXH3-128 digest of `input`.
#[must_use]
#[inline]
pub fn hash_bytes(input: &[u8]) -> Digest {
    xxhash_rust::xxh3::xxh3_128(input)
}

/// Computes the XXH3-128 digest of a string slice.
#[must_use]
#[inline]
pub fn hash_str(input: &str) -> Digest {
    hash_bytes(input.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xxh3_128_matches_reference_for_chunked_input() {
        let input = b"rezzy-xxh3";

        let direct = hash_bytes(input);

        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        hasher.update(&input[..4]);
        hasher.update(&input[4..]);
        let streaming = hasher.digest128();

        assert_eq!(direct, streaming);
    }
}
