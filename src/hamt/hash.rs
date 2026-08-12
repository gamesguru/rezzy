use blake2::{digest::Mac, Blake2bMac512};
use core::hash::Hasher;

/// A 128-bit structural hash for HAMT nodes.
///
/// This is a local cache key used to skip identical subtrees across HAMT
/// instances. It is not a wire format.
pub type StructuralHash = [u8; 16];

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

/// Computes the 128-bit keyed structural hash for an internal node based on
/// its bitmap and children.
///
/// # Panics
/// Panics if the keyed MAC constructor rejects the provided key.
#[allow(dead_code)]
pub(crate) fn compute_structural_hash(
    key: &[u8],
    datamap: u32,
    nodemap: u32,
    children_hashes: &[StructuralHash],
) -> StructuralHash {
    let mut mac = StructuralHashBuilder::new(key);
    mac.write(&datamap.to_le_bytes());
    mac.write(&nodemap.to_le_bytes());
    for h in children_hashes {
        mac.write(h);
    }
    mac.finish()
}
