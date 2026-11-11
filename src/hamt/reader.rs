//! Cold-storage reader for persisted HAMT nodes.
//!
//! A persisted CHAMP is a content-addressed tree of `MTHN` records plus a root
//! identity (see [`crate::hamt::state_hamt`] for the Matrix state-HAMT root
//! record). This reader resolves only the nodes a traversal touches, verifies
//! each decoded node against the structural hash that selected it, and caches
//! resolved nodes for the duration of the read.
//!
//! It deliberately takes a byte loader instead of a storage trait: callers
//! already know how to fetch a record by structural hash, and keeping the
//! dependency out of this module lets the same reader serve the CLI, a
//! homeserver, and tests.
//!
//! See [`crate::hamt`] for the deferred lazy-resolver and typed-root notes.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::borrow::Borrow;
use core::fmt;
use core::hash::Hash;

use super::codec::PersistedInternalNode;
use super::{HamtCodec, HamtNode, NodeRef, StructuralHash, HAMT_MAX_DEPTH};

/// Why a persisted HAMT could not be read.
#[derive(Debug)]
pub enum HamtReadError<E> {
    /// The caller's storage callback failed.
    Load(E),
    /// The root, or one of its children, has no record in storage.
    ///
    /// A referenced-but-absent node is corruption: a persisted root is only
    /// published after every node it names is durable.
    Missing(StructuralHash),
    /// The record was not a valid v1 node, or its bytes did not reproduce the
    /// structural hash that selected it.
    Decode(&'static str),
}

impl<E: fmt::Display> fmt::Display for HamtReadError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(f, "persisted HAMT load failed: {error}"),
            Self::Missing(hash) => {
                write!(f, "persisted HAMT node ")?;
                for byte in hash {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, " is missing")
            }
            Self::Decode(message) => {
                write!(f, "persisted HAMT node is invalid: {message}")
            }
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for HamtReadError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Load(error) => Some(error),
            Self::Missing(_) | Self::Decode(_) => None,
        }
    }
}

/// A lazily resolved, verified view over one persisted HAMT.
///
/// `load` receives a structural hash and returns the encoded `MTHN` record, or
/// `None` when no such record exists. `structural_key` is the same key the
/// tree was built with: it is mixed into every structural hash, so a node
/// decoded under the wrong key fails verification instead of silently
/// producing a different tree.
///
/// Resolved nodes are cached, so repeated lookups and a full walk share one
/// decode per node. The cache lives as long as the reader.
pub struct PersistedHamtReader<'k, K, V, E, L>
where
    K: HamtCodec + Hash,
    V: HamtCodec + Hash,
    L: FnMut(&StructuralHash) -> Result<Option<Vec<u8>>, E>,
{
    structural_key: &'k [u8],
    load: L,
    cache: BTreeMap<StructuralHash, Arc<HamtNode<K, V>>>,
}

impl<'k, K, V, E, L> PersistedHamtReader<'k, K, V, E, L>
where
    K: HamtCodec + Hash,
    V: HamtCodec + Hash,
    L: FnMut(&StructuralHash) -> Result<Option<Vec<u8>>, E>,
{
    /// Open a reader over a tree root baked with `structural_key`.
    #[must_use]
    pub fn new(structural_key: &'k [u8], load: L) -> Self {
        Self {
            structural_key,
            load,
            cache: BTreeMap::new(),
        }
    }

    /// Load and verify the node with `hash`, reusing a cached decode.
    fn node(&mut self, hash: &StructuralHash) -> Result<Arc<HamtNode<K, V>>, HamtReadError<E>> {
        if let Some(node) = self.cache.get(hash) {
            return Ok(Arc::clone(node));
        }
        let bytes = (self.load)(hash).map_err(HamtReadError::Load)?;
        let bytes = bytes.ok_or(HamtReadError::Missing(*hash))?;
        let node =
            PersistedInternalNode::<K, V>::decode_v1_verified(&bytes, self.structural_key, *hash)
                .map_err(HamtReadError::Decode)?;
        let node = Arc::new(node);
        self.cache.insert(*hash, Arc::clone(&node));
        Ok(node)
    }

    /// Look up one key under `root`.
    /// Look up one key under `root`, routing with the default key hash.
    ///
    /// For trees built with a custom routing hash use
    /// [`Self::get_with_key_hash`].
    ///
    /// `root` is the structural hash of the root node, not the state-group id
    /// from the root record. Callers that hold a root record should pass its
    /// [`StateHamtRoot::root_hash`](super::state_hamt::StateHamtRoot::root_hash).
    ///
    /// # Errors
    /// Returns [`HamtReadError`] when a touched node is missing, unreadable,
    /// or fails structural-hash verification.
    pub fn get<Q>(&mut self, root: StructuralHash, key: &Q) -> Result<Option<V>, HamtReadError<E>>
    where
        K: Eq + Borrow<Q>,
        V: Clone,
        Q: Hash + Eq + ?Sized,
    {
        let root_node = self.node(&root)?;
        // Copy the reference before the resolver closure borrows `self`.
        let structural_key = self.structural_key;
        let mut resolver = |hash: &StructuralHash| self.node(hash);
        root_node.search(structural_key, key, &mut resolver)
    }

    /// Look up one key under `root` in a tree built with a custom routing hash
    /// (see [`build_hamt_with_key_hash`](super::build_hamt_with_key_hash)).
    ///
    /// `key_hash` must be the exact function the tree was built with, e.g.
    /// [`canonical_key_path_hash`](super::canonical_key_path_hash).
    ///
    /// # Errors
    /// or fails structural-hash verification.
    pub fn get_with_key_hash<Q, H>(
        &mut self,
        root: StructuralHash,
        key: &Q,
        key_hash: H,
    ) -> Result<Option<V>, HamtReadError<E>>
    where
        K: Eq + Borrow<Q>,
        V: Clone,
        Q: Eq + ?Sized,
        H: FnMut(&Q) -> StructuralHash,
    {
        let root_node = self.node(&root)?;
        let mut resolver = |hash: &StructuralHash| self.node(hash);
        root_node.search_with_key_hash(key, key_hash, &mut resolver)
    }

    /// Look up each key under `root`, in order.
    ///
    /// # Errors
    /// Returns [`HamtReadError`] on the first node that cannot be read.
    pub fn get_many(
        &mut self,
        root: StructuralHash,
        keys: &[K],
    ) -> Result<Vec<Option<V>>, HamtReadError<E>>
    where
        K: Eq,
        V: Clone,
    {
        let mut values = Vec::with_capacity(keys.len());
        for key in keys {
            values.push(self.get(root, key)?);
        }
        Ok(values)
    }

    /// Visit every key/value pair under `root` in trie order.
    ///
    /// # Errors
    /// Returns [`HamtReadError`] when a touched node is missing or invalid.
    pub fn for_each_entry<F>(
        &mut self,
        root: StructuralHash,
        mut visit: F,
    ) -> Result<(), HamtReadError<E>>
    where
        F: FnMut(&K, &V),
    {
        // Iterative walk with an explicit depth bound, so a malformed chain of
        // persisted nodes errors instead of overflowing the stack.
        let mut stack = vec![(self.node(&root)?, 0_usize)];
        while let Some((node, depth)) = stack.pop() {
            if depth >= HAMT_MAX_DEPTH {
                return Err(HamtReadError::Decode("HAMT exceeds maximum depth"));
            }
            for (key, value) in &node.leaves {
                visit(key, value);
            }
            for child in &node.children {
                let child = match child {
                    NodeRef::Resolved(child) => Arc::clone(child),
                    NodeRef::Lazy(hash) => self.node(hash)?,
                };
                stack.push((child, depth.saturating_add(1)));
            }
        }
        Ok(())
    }

    /// Collect every key/value pair under `root`.
    ///
    /// # Errors
    /// Returns [`HamtReadError`] when a touched node is missing or invalid.
    pub fn collect_entries(&mut self, root: StructuralHash) -> Result<Vec<(K, V)>, HamtReadError<E>>
    where
        K: Clone,
        V: Clone,
    {
        let mut entries = Vec::new();
        self.for_each_entry(root, |key, value| {
            entries.push((key.clone(), value.clone()));
        })?;
        Ok(entries)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::hamt::{build_hamt_root_handle, NodeRef};
    use crate::incremental::LtHash;
    use alloc::string::{String, ToString};
    use alloc::vec;

    const KEY: &[u8] = b"reader-test-room";

    /// Build a small tree and return `(records, root_hash)` in the shape a
    /// storage loader would see.
    fn fixture() -> (BTreeMap<StructuralHash, Vec<u8>>, StructuralHash) {
        let entries = vec![
            ("alpha".to_string(), "1".to_string()),
            ("beta".to_string(), "2".to_string()),
            ("gamma".to_string(), "3".to_string()),
        ];
        let (handle, root) =
            build_hamt_root_handle(KEY, &LtHash::default(), entries).expect("build fixture");
        let mut records = BTreeMap::new();
        collect(&root, &mut records);
        (records, handle.structural_hash)
    }

    /// Build a reader over `records` with the given structural key, so tests
    /// do not repeat the same loader closure.
    #[allow(clippy::type_complexity)]
    fn reader_for<'a>(
        records: &'a BTreeMap<StructuralHash, Vec<u8>>,
        structural_key: &'a [u8],
    ) -> PersistedHamtReader<
        'a,
        String,
        String,
        (),
        impl FnMut(&StructuralHash) -> Result<Option<Vec<u8>>, ()> + 'a,
    > {
        PersistedHamtReader::new(structural_key, move |hash| Ok(records.get(hash).cloned()))
    }

    fn collect(
        node: &Arc<HamtNode<String, String>>,
        records: &mut BTreeMap<StructuralHash, Vec<u8>>,
    ) {
        if records.contains_key(&node.structural_hash) {
            return;
        }
        for child in &node.children {
            if let NodeRef::Resolved(child) = child {
                collect(child, records);
            }
        }
        let persisted: PersistedInternalNode<String, String> = node.as_ref().into();
        records.insert(node.structural_hash, persisted.encode_v1());
    }

    #[test]
    fn gets_and_collects_from_persisted_nodes() {
        let (records, root) = fixture();
        let mut reader = reader_for(&records, KEY);

        assert_eq!(reader.get(root, "beta").unwrap().as_deref(), Some("2"));
        assert_eq!(reader.get(root, "absent").unwrap(), None);
        assert_eq!(
            reader
                .get_many(root, &["alpha".to_string(), "gamma".to_string()])
                .unwrap(),
            vec![Some("1".to_string()), Some("3".to_string())]
        );

        let mut entries = reader.collect_entries(root).unwrap();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                ("alpha".to_string(), "1".to_string()),
                ("beta".to_string(), "2".to_string()),
                ("gamma".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn a_missing_node_is_reported_not_skipped() {
        let (records, _root) = fixture();
        let mut reader = reader_for(&records, KEY);
        let missing = [0xAA; 32];
        match reader.get(missing, "alpha") {
            Err(HamtReadError::Missing(hash)) => assert_eq!(hash, missing),
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_or_miskeyed_nodes_fail_verification() {
        let (mut records, root) = fixture();
        records.insert(root, vec![0_u8; 12]);
        let mut reader = reader_for(&records, KEY);
        assert!(matches!(
            reader.get(root, "alpha"),
            Err(HamtReadError::Decode(_))
        ));

        // The same bytes under a different structural key do not reproduce
        // the hash that selected them.
        let (records, root) = fixture();
        let mut reader = reader_for(&records, b"wrong-room");
        assert!(matches!(
            reader.get(root, "alpha"),
            Err(HamtReadError::Decode(_))
        ));
    }

    #[test]
    fn loader_errors_are_preserved() {
        let (_, root) = fixture();
        let mut reader = PersistedHamtReader::<String, String, _, _>::new(KEY, |_| Err("disk"));
        match reader.get(root, "alpha") {
            Err(HamtReadError::Load("disk")) => {}
            other => panic!("expected Load, got {other:?}"),
        }
    }
}
