//! Generic HAMT primitives used by higher-level state handling.
//!
//! Layout:
//! - `hash`: keyed structural hashing for subtree identity
//! - `codec`: dense on-disk encoding for persisted internal nodes
//! - `delta`: subtree differencing for set isolation
//! - `tests`: regression coverage for the generic HAMT core

use alloc::{sync::Arc, vec::Vec};
use core::hash::Hash;

pub mod codec;
pub mod delta;
pub mod hash;

#[cfg(test)]
mod tests;

pub use codec::PersistedInternalNode;
pub use hash::{state_group_id_from_lthash, RootHandle, StateGroupId, StructuralHash};

use hash::StructuralHashBuilder;

/// A node in the 32-way CHAMP (Compressed Hash Array Mapped Prefix) trie.
#[derive(Clone, Debug)]
pub enum HamtNode<K, V> {
    /// An internal node carrying child pointers and a structural hash.
    Internal {
        /// Bitmap marking which of the 32 slots contain leaf data.
        datamap: u32,
        /// Bitmap marking which of the 32 slots contain child internal nodes.
        nodemap: u32,
        /// The compressed array of child nodes (leaves and internal nodes
        /// combined). Length matches `datamap.count_ones() +
        /// nodemap.count_ones()`.
        children: Vec<Arc<Self>>,
        /// Structural hash for O(1) subtree equivalence checks.
        structural_hash: StructuralHash,
    },
    /// A leaf node containing a key-value pair.
    Leaf { key: K, value: V },
}

impl<K, V> HamtNode<K, V> {
    /// Returns the structural hash of this node using the given key.
    ///
    /// # Panics
    /// Panics if the keyed MAC constructor rejects the provided key.
    pub fn structural_hash(&self, key: &[u8]) -> StructuralHash
    where
        K: Hash,
        V: Hash,
    {
        match self {
            Self::Internal {
                structural_hash, ..
            } => *structural_hash,
            Self::Leaf { key: k, value: v } => {
                let mut mac = StructuralHashBuilder::new(key);
                k.hash(&mut mac);
                v.hash(&mut mac);
                mac.finish()
            }
        }
    }
}
