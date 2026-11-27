//! Cold-storage reader for persisted HAMT nodes.
//!
//! A persisted CHAMP is a content-addressed tree of `MTHN` records plus a root
//! identity (see [`crate::state::hamt_root`] for the Matrix state-HAMT root
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

use super::codec::PersistedInternalNode;
use super::{map_index, HamtCodec, HamtNode, NodeRef, StructuralHash, HAMT_MAX_DEPTH};

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
    K: HamtCodec,
    V: HamtCodec,
    L: FnMut(&StructuralHash) -> Result<Option<Vec<u8>>, E>,
{
    structural_key: &'k [u8],
    load: L,
    route: fn(&K, &[u8]) -> StructuralHash,
    cache: BTreeMap<StructuralHash, Arc<HamtNode<K, V>>>,
}

/// Production routing: the keyed structural hash used by every public tree
/// builder.
fn default_route<K: HamtCodec>(key: &K, structural_key: &[u8]) -> StructuralHash {
    super::key_path_hash(structural_key, key)
}

/// Validates that every leaf in a terminal collision bucket routes to the same
/// path hash, returning that hash.
impl<'k, K, V, E, L> PersistedHamtReader<'k, K, V, E, L>
where
    K: HamtCodec,
    V: HamtCodec,
    L: FnMut(&StructuralHash) -> Result<Option<Vec<u8>>, E>,
{
    /// Open a reader over a tree root baked with `structural_key`.
    #[must_use]
    pub fn new(structural_key: &'k [u8], load: L) -> Self {
        Self {
            structural_key,
            load,
            route: default_route::<K>,
            cache: BTreeMap::new(),
        }
    }

    /// Test-only constructor with an injectable routing hash, matching the
    /// `build_hamt_with_routing` fixture hook.
    #[cfg(test)]
    pub(crate) fn new_with_route(
        structural_key: &'k [u8],
        load: L,
        route: fn(&K, &[u8]) -> StructuralHash,
    ) -> Self {
        Self {
            structural_key,
            load,
            route,
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

    /// Look up one key under `root`, routing with the default key hash.
    ///
    /// `root` is the structural hash of the root node, not the state-group id
    /// from the root record. Callers that hold a root record should pass its
    /// [`StateHamtRoot::root_hash`](crate::state::hamt_root::StateHamtRoot::root_hash).
    ///
    /// # Errors
    /// Returns [`HamtReadError`] when a touched node is missing, unreadable,
    /// or fails structural-hash verification.
    pub fn get<Q>(&mut self, root: StructuralHash, key: &Q) -> Result<Option<V>, HamtReadError<E>>
    where
        K: Eq + Borrow<Q>,
        V: Clone,
        Q: super::HamtRoutingKey + Eq + ?Sized,
    {
        let path_hash = super::key_path_hash(self.structural_key, key);
        self.get_by_path_hash(root, key, &path_hash)
    }

    /// Look up one key using a previously computed routing hash.
    ///
    /// This is useful when the caller already has the same path hash used to
    /// build the tree. `get` is simpler for ordinary reads.
    ///
    /// # Errors
    /// Returns [`HamtReadError`] when a touched node is missing, unreadable,
    /// or fails structural-hash verification.
    pub fn get_by_path_hash<Q>(
        &mut self,
        root: StructuralHash,
        key: &Q,
        path_hash: &StructuralHash,
    ) -> Result<Option<V>, HamtReadError<E>>
    where
        K: Eq + Borrow<Q>,
        V: Clone,
        Q: Eq + ?Sized,
    {
        let route = self.route;
        let structural_key = self.structural_key;
        let root_node = self.node(&root)?;
        let mut resolver = |hash: &StructuralHash| self.node(hash);
        let mut validate_collision = |leaves: &[(K, V)], expected: &StructuralHash| {
            let actual = super::collision_route_hash(leaves, |key| route(key, structural_key))?;
            if actual == *expected {
                Ok(())
            } else {
                Err("collision bucket routes to a different path hash")
            }
        };
        root_node
            .search_by_path_hash_checked(key, path_hash, &mut resolver, &mut validate_collision)
            .map_err(|error| match error {
                super::HamtTraversalError::Resolve(error) => error,
                super::HamtTraversalError::CorruptNode(reason) => HamtReadError::Decode(reason),
                super::HamtTraversalError::MaxDepthExceeded { depth }
                    if depth == super::HAMT_MAX_DEPTH =>
                {
                    HamtReadError::Decode("terminal node is not a collision bucket")
                }
                super::HamtTraversalError::MaxDepthExceeded { .. } => {
                    HamtReadError::Decode("HAMT exceeds maximum depth")
                }
            })
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
        enum Pending<K, V> {
            Node(Arc<HamtNode<K, V>>, usize),
            Entry(Arc<HamtNode<K, V>>, usize, usize),
        }

        // Iterative walk with an explicit depth bound, so a malformed chain of
        // persisted nodes errors instead of overflowing the stack. Pending
        // entries are pushed in descending slot order, so popping interleaves
        // leaves and child subtrees in the node's actual trie-slot order.
        let route = self.route;
        let structural_key = self.structural_key;
        let mut stack = vec![Pending::Node(self.node(&root)?, 0_usize)];
        while let Some(pending) = stack.pop() {
            match pending {
                Pending::Node(node, depth) => {
                    if let Some(reason) = node.shape_error() {
                        return Err(HamtReadError::Decode(reason));
                    }
                    if node.is_collision() {
                        if depth != super::HAMT_MAX_DEPTH {
                            return Err(HamtReadError::Decode("misplaced collision bucket"));
                        }
                        super::collision_route_hash(&node.leaves, |key| route(key, structural_key))
                            .map_err(|_| {
                                HamtReadError::Decode(
                                    "collision bucket leaves route to different path hashes",
                                )
                            })?;
                        for (key, value) in &node.leaves {
                            visit(key, value);
                        }
                        continue;
                    }
                    if depth >= HAMT_MAX_DEPTH {
                        return Err(HamtReadError::Decode("HAMT exceeds maximum depth"));
                    }
                    for slot in (0..super::HAMT_BRANCH_FACTOR).rev() {
                        let bit = 1_u32 << slot;
                        if (node.datamap | node.nodemap) & bit != 0 {
                            stack.push(Pending::Entry(Arc::clone(&node), depth, slot));
                        }
                    }
                }
                Pending::Entry(node, depth, slot) => {
                    let bit = 1_u32 << slot;
                    if node.datamap & bit != 0 {
                        let index = map_index(node.datamap, slot);
                        let (key, value) = &node.leaves[index];
                        visit(key, value);
                    } else {
                        let index = map_index(node.nodemap, slot);
                        let child = match &node.children[index] {
                            NodeRef::Resolved(child) => Arc::clone(child),
                            NodeRef::Lazy(hash) => self.node(hash)?,
                        };
                        stack.push(Pending::Node(child, depth.saturating_add(1)));
                    }
                }
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
        let (handle, root) = build_hamt_root_handle(KEY, &LtHash::default(), entries);
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

    fn collect<K: HamtCodec + Clone, V: HamtCodec + Clone>(
        node: &Arc<HamtNode<K, V>>,
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
        let persisted: PersistedInternalNode<K, V> = node.as_ref().into();
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
        entries.sort_unstable();
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
    fn reads_terminal_collision_buckets() {
        let root = crate::hamt::build_hamt_with_routing(
            KEY,
            vec![(1_u64, 10_u64), (2_u64, 20_u64), (3_u64, 30_u64)],
            |_| [0_u8; 32],
        );
        let mut records = BTreeMap::new();
        collect(&root, &mut records);

        let mut reader: PersistedHamtReader<'_, u64, u64, (), _> =
            PersistedHamtReader::new_with_route(
                KEY,
                |hash| Ok(records.get(hash).cloned()),
                |_, _| [0_u8; 32],
            );
        assert_eq!(
            reader
                .get_by_path_hash(root.structural_hash, &2, &[0_u8; 32])
                .unwrap(),
            Some(20)
        );
        assert_eq!(
            reader
                .get_by_path_hash(root.structural_hash, &3, &[0_u8; 32])
                .unwrap(),
            Some(30)
        );
        assert_eq!(
            reader
                .get_by_path_hash(root.structural_hash, &9, &[0_u8; 32])
                .unwrap(),
            None
        );

        let mut entries = reader.collect_entries(root.structural_hash).unwrap();
        entries.sort_unstable();
        assert_eq!(entries, vec![(1, 10), (2, 20), (3, 30)]);
    }

    #[test]
    fn rejects_collision_bucket_with_inconsistent_routing() {
        let root = crate::hamt::build_hamt_with_routing(
            KEY,
            vec![(1_u64, 10_u64), (2_u64, 20_u64)],
            |_| [0_u8; 32],
        );
        let mut records = BTreeMap::new();
        collect(&root, &mut records);

        // Production routing is the default keyed hash, not the synthetic
        // constant route the fixture was built with. The bucket's leaves
        // therefore disagree about their route and must be rejected.
        let mut reader: PersistedHamtReader<'_, u64, u64, (), _> =
            PersistedHamtReader::new(KEY, |hash| Ok(records.get(hash).cloned()));
        assert!(matches!(
            reader.get_by_path_hash(root.structural_hash, &1, &[0_u8; 32]),
            Err(HamtReadError::Decode("leaf routed to wrong slot"))
        ));
    }

    #[test]
    fn rejects_misplaced_collision_bucket_root() {
        let bucket =
            crate::hamt::rebuild_node(KEY, 0, 0, vec![(1_u64, 10_u64), (2_u64, 20_u64)], vec![]);
        let mut records = BTreeMap::new();
        collect(&bucket, &mut records);

        let mut reader: PersistedHamtReader<'_, u64, u64, (), _> =
            PersistedHamtReader::new(KEY, |hash| Ok(records.get(hash).cloned()));
        assert!(matches!(
            reader.get_by_path_hash(bucket.structural_hash, &1, &[0_u8; 32]),
            Err(HamtReadError::Decode("misplaced collision bucket"))
        ));
    }

    #[test]
    fn get_by_path_hash_rejects_misplaced_and_nonterminal_end_nodes() {
        let misplaced_leaves = vec![(1_u64, 10_u64), (2_u64, 20_u64)];
        let misplaced_hash = HamtNode::compute_structural_hash(KEY, 0, 0, &misplaced_leaves, &[]);
        let misplaced = Arc::new(HamtNode {
            datamap: 0,
            nodemap: 0,
            leaves: misplaced_leaves,
            children: vec![],
            structural_hash: misplaced_hash,
        });
        let mut misplaced_records = BTreeMap::new();
        collect(&misplaced, &mut misplaced_records);
        let mut misplaced_reader: PersistedHamtReader<'_, u64, u64, (), _> =
            PersistedHamtReader::new_with_route(
                KEY,
                |hash| Ok(misplaced_records.get(hash).cloned()),
                |_, _| [0_u8; 32],
            );
        assert!(matches!(
            misplaced_reader.get_by_path_hash(misplaced_hash, &1, &[0_u8; 32]),
            Err(HamtReadError::Decode("misplaced collision bucket"))
        ));

        // Construct a structurally valid ordinary node at depth MAX. Its
        // bytes and hash verify, but it cannot legally terminate a routed path.
        let empty_hash = HamtNode::<u64, u64>::compute_structural_hash(KEY, 0, 0, &[], &[]);
        let mut terminal = Arc::new(HamtNode::<u64, u64> {
            datamap: 0,
            nodemap: 0,
            leaves: vec![],
            children: vec![],
            structural_hash: empty_hash,
        });
        for _ in 0..HAMT_MAX_DEPTH {
            let children = vec![NodeRef::Resolved(Arc::clone(&terminal))];
            let hash = HamtNode::compute_structural_hash(KEY, 0, 1, &[], &children);
            terminal = Arc::new(HamtNode {
                datamap: 0,
                nodemap: 1,
                leaves: vec![],
                children,
                structural_hash: hash,
            });
        }
        let mut terminal_records = BTreeMap::new();
        collect(&terminal, &mut terminal_records);
        let mut terminal_reader: PersistedHamtReader<'_, u64, u64, (), _> =
            PersistedHamtReader::new(KEY, |hash| Ok(terminal_records.get(hash).cloned()));
        assert!(matches!(
            terminal_reader.get_by_path_hash(terminal.structural_hash, &1, &[0_u8; 32]),
            Err(HamtReadError::Decode(
                "terminal node is not a collision bucket"
            ))
        ));
    }

    #[test]
    fn entries_are_visited_in_trie_slot_order() {
        // A hand-built root with leaves in slots 0 and 4 and a child subtree in
        // slot 2. A leaves-then-children walk would visit both leaves first.
        let child = Arc::new(HamtNode {
            datamap: 1,
            nodemap: 0,
            leaves: vec![("child".to_string(), "c".to_string())],
            children: Vec::new(),
            structural_hash: HamtNode::compute_structural_hash(
                KEY,
                1,
                0,
                &[("child".to_string(), "c".to_string())],
                &[],
            ),
        });
        let children = vec![NodeRef::Resolved(Arc::clone(&child))];
        let leaves = vec![
            ("first".to_string(), "1".to_string()),
            ("last".to_string(), "2".to_string()),
        ];
        let root_node = Arc::new(HamtNode {
            datamap: (1 << 0) | (1 << 4),
            nodemap: 1 << 2,
            leaves,
            children,
            structural_hash: HamtNode::compute_structural_hash(
                KEY,
                (1 << 0) | (1 << 4),
                1 << 2,
                &[
                    ("first".to_string(), "1".to_string()),
                    ("last".to_string(), "2".to_string()),
                ],
                &[NodeRef::Resolved(child)],
            ),
        });

        let mut records = BTreeMap::new();
        collect(&root_node, &mut records);
        let mut reader = reader_for(&records, KEY);
        let mut order = Vec::new();
        reader
            .for_each_entry(root_node.structural_hash, |key, _| {
                order.push(key.clone());
            })
            .unwrap();
        assert_eq!(order, ["first", "child", "last"]);
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
