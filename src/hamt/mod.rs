//! Generic HAMT primitives used by higher-level state handling.
//!
//! Layout:
//! - `hash`: keyed structural hashing for subtree identity
//! - `codec`: dense on-disk encoding for persisted internal nodes
//! - `delta`: subtree differencing for set isolation
//! - `tests`: regression coverage for the generic HAMT core

use alloc::{sync::Arc, vec::Vec};
use core::hash::{Hash, Hasher};

pub mod codec;
pub mod delta;
pub mod hash;

#[cfg(test)]
mod tests;

pub use codec::PersistedInternalNode;
pub use hash::{state_group_id_from_lthash, RootHandle, StateGroupId, StructuralHash};

use hash::StructuralHashBuilder;

/// A reference to a child node in the HAMT.
#[derive(Clone, Debug)]
pub enum NodeRef<K, V> {
    /// A fully loaded child node.
    Resolved(Arc<HamtNode<K, V>>),
    /// A lazy-loaded child node that hasn't been fetched from storage yet.
    Lazy(StructuralHash),
}

impl<K, V> NodeRef<K, V> {
    /// Gets the structural hash of the child node without loading it.
    #[must_use]
    pub fn structural_hash(&self) -> StructuralHash {
        match self {
            Self::Resolved(node) => node.structural_hash,
            Self::Lazy(hash) => *hash,
        }
    }
}

/// A node in the 32-way CHAMP (Compressed Hash Array Mapped Prefix) trie.
#[derive(Clone, Debug)]
pub struct HamtNode<K, V> {
    /// Bitmap marking which of the 32 slots contain leaf data.
    pub datamap: u32,
    /// Bitmap marking which of the 32 slots contain child internal nodes.
    pub nodemap: u32,
    /// The inline array of leaf key-value pairs. Length matches `datamap.count_ones()`.
    pub leaves: Vec<(K, V)>,
    /// The array of child nodes. Length matches `nodemap.count_ones()`.
    pub children: Vec<NodeRef<K, V>>,
    /// Structural hash for O(1) subtree equivalence checks.
    pub structural_hash: StructuralHash,
}

impl<K, V> HamtNode<K, V> {
    /// Computes the structural hash of this node from its contents.
    ///
    /// # Panics
    /// Panics if the keyed MAC constructor rejects the provided key.
    pub fn compute_structural_hash(
        key: &[u8],
        datamap: u32,
        nodemap: u32,
        leaves: &[(K, V)],
        children: &[NodeRef<K, V>],
    ) -> StructuralHash
    where
        K: Hash,
        V: Hash,
    {
        let mut mac = StructuralHashBuilder::new(key);
        mac.write(&datamap.to_le_bytes());
        mac.write(&nodemap.to_le_bytes());
        for (k, v) in leaves {
            k.hash(&mut mac);
            v.hash(&mut mac);
        }
        for child in children {
            mac.write(&child.structural_hash());
        }
        mac.finish()
    }
}
