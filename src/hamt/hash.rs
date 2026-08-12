use blake2::{digest::Mac, Blake2bMac512};
use core::hash::Hasher;

/// A 128-bit structural hash for HAMT nodes.
///
/// This is a local cache key used to skip identical subtrees across HAMT
/// instances. It is not a wire format.
pub type StructuralHash = [u8; 16];

/// A 32-byte state-group identifier derived from the full root lattice.
///
/// This is the cross-server, deduplicable identifier for a resolved root. It
/// must not be confused with the local-only `StructuralHash`.
pub type StateGroupId = [u8; 32];

/// A resolved root handle carrying both the local structural hash and the
/// global state-group identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootHandle {
    pub structural_hash: StructuralHash,
    pub state_group_id: StateGroupId,
}

impl RootHandle {
    /// Builds a root handle from a precomputed structural hash and a state
    /// lattice.
    #[must_use]
    pub fn from_lthash(structural_hash: StructuralHash, lattice: &crate::state::LtHash) -> Self {
        Self {
            structural_hash,
            state_group_id: state_group_id_from_lthash(lattice),
        }
    }
}

pub(crate) struct StructuralHashBuilder(Blake2bMac512);

impl StructuralHashBuilder {
    pub(crate) fn new(key: &[u8]) -> Self {
        Self(Blake2bMac512::new_from_slice(key).expect("Blake2b takes any key size"))
    }

    pub(crate) fn finish(self) -> StructuralHash {
        let result = self.0.finalize().into_bytes();
        let mut out = [0_u8; 16];
        out.copy_from_slice(&result[..16]);
        out
    }
}

impl Hasher for StructuralHashBuilder {
    fn finish(&self) -> u64 {
        0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

/// Computes the 32-byte state-group identifier from the full resolved lattice.
///
/// This uses the `LtHash` checksum, which is `BLAKE2b-256(lattice)`.
#[must_use]
pub fn state_group_id_from_lthash(lattice: &crate::state::LtHash) -> StateGroupId {
    lattice.checksum()
}
