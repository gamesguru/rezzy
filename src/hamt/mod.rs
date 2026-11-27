//! Content-addressed persistent HAMT (CHAMP), generic over key and value types.
//!
//! Keys and values are any types with a [`HamtCodec`] encoding. Nodes are
//! immutable records addressed by a keyed BLAKE3 structural hash, writes
//! path-copy and share every unchanged subtree, and nothing here knows what a
//! key or value means. The Matrix room-state root record built on it lives in
//! [`crate::state::hamt_root`].
//!
//! Layout:
//! - `hash`: keyed structural hashing for subtree identity
//! - `codec`: dense on-disk encoding for persisted internal nodes, and the
//!   routing bytes of a key
//! - `delta`: subtree differencing for set isolation
//! - `audit`: multi-root reachability audit for storage GC
//! - `reader`: lazy, verified reads of persisted nodes
//! - `gc`: experimental incremental refcount bookkeeping for integrations
//!   with a strictly linear root history (behind `unstable-refcount-gc`)
//! - `tests`: regression coverage for the generic HAMT core
//!
//! # Routing
//!
//! Every default-routed operation hashes a key's [`HamtRoutingKey`] bytes,
//! which for a [`HamtCodec`] type are its persisted encoding ([`key_path_hash`]).
//! A tree's shape therefore depends only on the encoded keys and the structural
//! key, never on a Rust `Hash` implementation, so the same entries give
//! byte-identical nodes in every process, version and architecture, and a
//! persisted tree can be read by anything that knows its codec. Lookups may
//! pass a borrowed key (`&str` for `String`) whose routing bytes match its
//! owned form. Callers with a precomputed path hash can use the
//! `*_by_path_hash` lookup entry points.
//!
//! # Release notes
//!
//! - **0.6.0**: the HAMT storage-GC reachability-audit API was renamed to
//!   disambiguate it from `resolve::reachability`'s unrelated event-DAG
//!   concept. `ReachabilityAudit` → [`NodeReachabilityAudit`],
//!   `reachability_audit` → [`node_reachability_audit`],
//!   `BitmapReachabilityAudit` → [`BitmapNodeReachabilityAudit`], and
//!   `bitmap_reachability_audit` → [`bitmap_node_reachability_audit`]. The old
//!   names were removed (no aliases remain), so downstream code must use the
//!   new names; at the time of the rename neither had any caller.

use alloc::{sync::Arc, vec, vec::Vec};
use core::{borrow::Borrow, fmt};

pub mod audit;
pub mod codec;
pub mod delta;
#[cfg(any(test, feature = "unstable-refcount-gc"))]
pub mod gc;
pub mod hash;
pub mod reader;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

pub use audit::{bitmap_node_reachability_audit, BitmapAuditError, BitmapNodeReachabilityAudit};
pub use audit::{
    node_reachability_audit, unreachable_node_hashes, IndexedUniverse, NodeReachabilityAudit,
    UniverseTooLarge,
};
pub use codec::{
    HamtCodec, HamtRoutingKey, HamtStrCodec, PersistedInternalNode, HAMT_NODE_MAGIC,
    HAMT_ROOT_MAGIC, HAMT_WIRE_VERSION,
};
pub use delta::{
    diff_hamt_nodes, diff_node_hashes, reachable_node_hashes, walk_reachable_node_hashes, Delta,
    DeltaResult, HamtTraversalError, NodeHashDelta,
};
#[cfg(feature = "unstable-refcount-gc")]
pub use gc::{LinearRootChain, RefcountTable, RefcountUnderflow};
pub use hash::{
    state_group_id_from_lthash, RootHandle, StateGroupId, StructuralHash, HAMT_CODEC_VERSION,
    HAMT_ROUTING_VERSION,
};
pub use reader::{HamtReadError, PersistedHamtReader};

/// Error from [`HamtNode::visit_entries`].
#[derive(Debug, PartialEq, Eq)]
pub enum HamtVisitError<ResolveE, VisitorE> {
    /// A lazy child could not be loaded.
    Resolve(ResolveE),
    /// The caller's visitor rejected an entry.
    Visitor(VisitorE),
    /// A node is malformed for its position in the tree.
    CorruptNode(&'static str),
    /// The walk exceeded the maximum HAMT depth.
    MaxDepthExceeded { depth: usize },
}

impl<ResolveE: fmt::Display, VisitorE: fmt::Display> fmt::Display
    for HamtVisitError<ResolveE, VisitorE>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(error) => write!(f, "hamt entry walk resolver failed: {error}"),
            Self::Visitor(error) => write!(f, "hamt entry visitor failed: {error}"),
            Self::CorruptNode(reason) => write!(f, "hamt entry walk corrupt node: {reason}"),
            Self::MaxDepthExceeded { depth } => {
                write!(f, "hamt entry walk exceeded max depth at {depth}")
            }
        }
    }
}

impl<ResolveE, VisitorE> core::error::Error for HamtVisitError<ResolveE, VisitorE>
where
    ResolveE: core::error::Error + 'static,
    VisitorE: core::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Resolve(error) => Some(error),
            Self::Visitor(error) => Some(error),
            Self::CorruptNode(_) | Self::MaxDepthExceeded { .. } => None,
        }
    }
}

/// 256-bit routing path hash for a key in the HAMT.
pub type KeyPathHash = StructuralHash;

/// Shared pointer to a (possibly interned) HAMT node.
pub(crate) type NodePtr<K, V> = Arc<HamtNode<K, V>>;

/// Resolves a child node's [`StructuralHash`] to that node, or returns `E` if
/// it cannot be fetched.
///
/// This is the callback every HAMT descent, diff, and mutation entry point
/// takes, named so callers can spell the bound once instead of repeating the
/// full `FnMut(&StructuralHash) -> Result<Arc<HamtNode<K, V>>, E>` signature
/// (e.g. to store a `Box<dyn NodeResolver<..>>` or write a generic wrapper).
///
/// It is blanket-implemented for every matching `FnMut` closure and carries no
/// methods, so callers pass ordinary closures and never implement it
/// themselves.
pub trait NodeResolver<K, V, E>: FnMut(&StructuralHash) -> Result<NodePtr<K, V>, E> {}

impl<K, V, E, F> NodeResolver<K, V, E> for F where
    F: FnMut(&StructuralHash) -> Result<NodePtr<K, V>, E>
{
}

/// A key storable in a HAMT: comparable, cloneable, and codec-able.
///
/// Blanket-implemented; callers never implement it themselves. Names the bound
/// shared by the HAMT descent, mutation, and persistence entry points.
pub trait HamtKey: Eq + Clone + HamtCodec {}

impl<K: Eq + Clone + HamtCodec> HamtKey for K {}

/// A value storable in a HAMT: cloneable and codec-able.
///
/// Blanket-implemented; callers never implement it themselves.
pub trait HamtValue: Clone + HamtCodec {}

impl<V: Clone + HamtCodec> HamtValue for V {}

/// The outcome of descending one level of a HAMT with a batch of requested keys.
#[derive(Debug, PartialEq, Eq)]
pub struct DescendResult<K, V> {
    /// Keys found at this level with their decoded key and value.
    ///
    /// The key is carried alongside the path hash because a terminal
    /// collision bucket can hold several distinct keys that share one
    /// `KeyPathHash`; a `(hash, value)` pair alone cannot distinguish them.
    pub found: Vec<(KeyPathHash, K, V)>,
    /// Keys proven to be absent at this level (the corresponding slot bit was clear).
    pub absent: Vec<KeyPathHash>,
    /// Keys that need to descend further (to `depth + 1`), grouped by the child node's `StructuralHash`.
    pub pending: Vec<(StructuralHash, Vec<KeyPathHash>)>,
}

/// Errors that can occur during level-synchronous descent over persisted node bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum DescendError {
    /// Node decoding failed (invalid version or truncated buffer).
    Decode(&'static str),
    /// Invariant violation: bitmap indicated a leaf or child slot, but the array index was missing.
    CorruptNode(&'static str),
}

impl fmt::Display for DescendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(msg) => write!(f, "hamt descend decode error: {msg}"),
            Self::CorruptNode(msg) => write!(f, "hamt descend corrupt node: {msg}"),
        }
    }
}

impl core::error::Error for DescendError {}

/// A single step in a published state-group chain.
pub struct ChainStep<K, V> {
    /// The resolved root at this step.
    pub root: NodePtr<K, V>,
    /// The structural hash of `root`.
    pub root_hash: StructuralHash,
    /// The value displaced by this mutation, if any (needed for lattice subtraction).
    pub displaced: Option<V>,
    /// Nodes created by this step relative to the IMMEDIATELY PRECEDING root.
    ///
    /// These nodes must be written to storage before this step's `state_group` is published.
    pub created: Vec<(StructuralHash, Vec<u8>)>,
}

use hash::StructuralHashBuilder;

const STRUCTURAL_HASH_DOMAIN_V1: &[u8] = b"rezzy:hamt:structural-hash:v1\0";

const HAMT_BRANCH_BITS: usize = 5;
const HAMT_BRANCH_FACTOR: usize = 1 << HAMT_BRANCH_BITS;
const HAMT_BRANCH_MASK: u16 = 0b1_1111;
const HAMT_MAX_DEPTH: usize =
    (core::mem::size_of::<StructuralHash>() * 8).div_ceil(HAMT_BRANCH_BITS);

/// A reference to a child node in the HAMT.
#[derive(Clone, Debug)]
pub enum NodeRef<K, V> {
    /// A fully loaded child node.
    Resolved(NodePtr<K, V>),
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
#[derive(Debug)]
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
    /// Returns whether this node is a terminal collision bucket (the MTHC
    /// sentinel shape: no slot maps, no children, and at least two inline
    /// entries).
    ///
    /// A valid bucket only ever appears at [`HAMT_MAX_DEPTH`]; callers that
    /// care about structural correctness must still check the depth.
    #[must_use]
    pub fn is_collision(&self) -> bool {
        self.datamap == 0 && self.nodemap == 0 && self.leaves.len() > 1 && self.children.is_empty()
    }

    /// Returns why this node's in-memory shape is internally inconsistent.
    ///
    /// A collision bucket is a valid shape on its own; whether it is legal at
    /// a particular depth is a separate check. This validates the bitmap/array
    /// correspondence every traversal relies on before it indexes anything.
    pub(crate) fn shape_error(&self) -> Option<&'static str> {
        if self.datamap & self.nodemap != 0 {
            return Some("datamap and nodemap overlap");
        }
        if self.is_collision() {
            return None;
        }
        if self.leaves.len() != self.datamap.count_ones() as usize {
            return Some("leaf count does not match datamap");
        }
        if self.children.len() != self.nodemap.count_ones() as usize {
            return Some("child count does not match nodemap");
        }
        None
    }

    /// Computes the structural hash of this node from its contents.
    ///
    pub fn compute_structural_hash(
        key: &[u8],
        datamap: u32,
        nodemap: u32,
        leaves: &[(K, V)],
        children: &[NodeRef<K, V>],
    ) -> StructuralHash
    where
        K: HamtCodec,
        V: HamtCodec,
    {
        let child_hashes = children
            .iter()
            .map(NodeRef::structural_hash)
            .collect::<Vec<_>>();
        let canonical_bytes =
            PersistedInternalNode::encode_v1_parts(datamap, nodemap, leaves, &child_hashes);
        let mut mac = StructuralHashBuilder::new(key);
        mac.update(STRUCTURAL_HASH_DOMAIN_V1);
        mac.update(&canonical_bytes);
        mac.finalize()
    }

    /// Looks up a key in a fully materialized HAMT.
    ///
    /// This is the cheapest read path and should be used when the tree is
    /// already fully resolved in memory.
    ///
    /// A malformed node shape is treated as a missing key because this API
    /// returns `Option`; persisted reads should use [`PersistedHamtReader`],
    /// which reports malformed depth/shape as an error.
    #[must_use]
    pub fn get<Q>(&self, structural_key: &[u8], key: &Q) -> Option<&V>
    where
        K: Eq + Borrow<Q>,
        Q: HamtRoutingKey + Eq + ?Sized,
    {
        let path_hash = key_path_hash(structural_key, key);
        self.get_by_path_hash(key, &path_hash)
    }

    /// Looks up a key using a caller-provided routing hash.
    ///
    /// This is the most general lookup entry point. Use it when the caller
    /// already knows the exact path hash that was used to build or mutate the
    /// tree.
    ///
    /// A malformed node shape is treated as a missing key; persisted reads
    /// should use [`PersistedHamtReader`], which reports corruption.
    #[must_use]
    pub fn get_by_path_hash<Q>(&self, key: &Q, path_hash: &StructuralHash) -> Option<&V>
    where
        K: Eq + Borrow<Q>,
        Q: Eq + ?Sized,
    {
        self.get_by_path_hash_inner(key, path_hash, 0)
    }

    /// Looks up a key in a HAMT that may contain lazy children.
    ///
    /// Lazy children are resolved through `resolver` as needed. The returned
    /// value is cloned so the lookup does not need to hold references into
    /// transient resolved child allocations.
    ///
    /// # Errors
    /// Returns the error from `resolver` if a lazy child cannot be loaded.
    /// A malformed node shape is treated as a missing key; use
    /// [`PersistedHamtReader`] when persisted-data corruption must be reported.
    pub fn search<Q, F, E>(
        &self,
        structural_key: &[u8],
        key: &Q,
        resolver: &mut F,
    ) -> Result<Option<V>, E>
    where
        K: Eq + Borrow<Q>,
        V: Clone,
        Q: HamtRoutingKey + Eq + ?Sized,
        F: NodeResolver<K, V, E>,
    {
        let path_hash = key_path_hash(structural_key, key);
        self.search_by_path_hash(key, &path_hash, resolver)
    }

    /// Looks up a key using a caller-provided routing hash.
    ///
    /// This is the most general lookup entry point. Use it when the caller
    /// already knows the exact path hash that was used to build or mutate the
    /// tree.
    ///
    /// # Errors
    /// Returns the error from `resolver` if a lazy child cannot be loaded.
    /// A malformed node shape is treated as a missing key; use
    /// [`PersistedHamtReader`] when persisted-data corruption must be reported.
    pub fn search_by_path_hash<Q, F, E>(
        &self,
        key: &Q,
        path_hash: &StructuralHash,
        resolver: &mut F,
    ) -> Result<Option<V>, E>
    where
        K: Eq + Borrow<Q>,
        V: Clone,
        Q: Eq + ?Sized,
        F: NodeResolver<K, V, E>,
    {
        match self.search_by_path_hash_checked(key, path_hash, resolver, &mut |_, _| Ok(())) {
            Ok(found) => Ok(found),
            Err(HamtTraversalError::Resolve(error)) => Err(error),
            Err(
                HamtTraversalError::CorruptNode(_) | HamtTraversalError::MaxDepthExceeded { .. },
            ) => {
                // Preserve the historical Option-like behavior of this API
                // for malformed in-memory trees.
                Ok(None)
            }
        }
    }

    /// Shared checked descent used by in-memory search and persisted reads.
    /// The collision validator lets persisted callers enforce routing without
    /// duplicating slot traversal or its shape checks.
    pub(crate) fn search_by_path_hash_checked<Q, F, E, C>(
        &self,
        key: &Q,
        path_hash: &StructuralHash,
        resolver: &mut F,
        validate_collision: &mut C,
    ) -> Result<Option<V>, HamtTraversalError<E>>
    where
        K: Eq + Borrow<Q>,
        V: Clone,
        Q: Eq + ?Sized,
        F: NodeResolver<K, V, E>,
        C: FnMut(&[(K, V)], &StructuralHash) -> Result<(), &'static str>,
    {
        // Iterative descent with a single owned cursor for resolved children.
        let mut cursor: Option<NodePtr<K, V>> = None;
        for depth in 0..=HAMT_MAX_DEPTH {
            let node: &HamtNode<K, V> = cursor.as_deref().unwrap_or(self);
            delta::check_node_depth(node, depth)?;
            if node.is_collision() {
                validate_collision(&node.leaves, path_hash)
                    .map_err(HamtTraversalError::CorruptNode)?;
                return Ok(node
                    .leaves
                    .iter()
                    .find(|(stored_key, _)| stored_key.borrow() == key)
                    .map(|(_, value)| value.clone()));
            }
            if depth >= HAMT_MAX_DEPTH {
                return Ok(None);
            }
            match node.slot_at(path_hash, depth) {
                Slot::Leaf((stored_key, value)) => {
                    return Ok((stored_key.borrow() == key).then(|| value.clone()));
                }
                Slot::Child(child) => cursor = Some(delta::resolve_node_checked(child, resolver)?),
                Slot::Empty => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Visits every key/value pair in the HAMT, resolving lazy children as
    /// needed.
    ///
    /// This is the reusable traversal primitive for callers that need to
    /// stream or collect the full contents of a subtree.
    ///
    /// # Errors
    /// Returns [`HamtVisitError::Resolve`] or [`HamtVisitError::Visitor`] for
    /// callback failures, and reports malformed node shape or depth instead
    /// of silently returning an incomplete collection.
    pub fn visit_entries<F, ResolveE, VisitorE>(
        &self,
        resolver: &mut F,
        visitor: &mut impl FnMut(&K, &V) -> Result<(), VisitorE>,
    ) -> Result<(), HamtVisitError<ResolveE, VisitorE>>
    where
        F: NodeResolver<K, V, ResolveE>,
    {
        self.visit_entries_inner(resolver, visitor, 0)
    }

    /// Depth-checked implementation of [`Self::visit_entries`].
    fn visit_entries_inner<F, ResolveE, VisitorE>(
        &self,
        resolver: &mut F,
        visitor: &mut impl FnMut(&K, &V) -> Result<(), VisitorE>,
        depth: usize,
    ) -> Result<(), HamtVisitError<ResolveE, VisitorE>>
    where
        F: NodeResolver<K, V, ResolveE>,
    {
        delta::check_node_depth(self, depth).map_err(|error| match error {
            HamtTraversalError::Resolve(error) => HamtVisitError::Resolve(error),
            HamtTraversalError::CorruptNode(reason) => HamtVisitError::CorruptNode(reason),
            HamtTraversalError::MaxDepthExceeded { depth } => {
                HamtVisitError::MaxDepthExceeded { depth }
            }
        })?;
        for (key, value) in &self.leaves {
            visitor(key, value).map_err(HamtVisitError::Visitor)?;
        }

        if self.is_collision() {
            return Ok(());
        }

        for child in &self.children {
            let child_node =
                delta::resolve_node(child, resolver).map_err(HamtVisitError::Resolve)?;
            child_node.visit_entries_inner(resolver, visitor, depth.saturating_add(1))?;
        }

        Ok(())
    }

    /// Returns `true` if this node contains no leaf entries and no child nodes.
    ///
    /// This is an O(1) structural check on the node's bitmaps.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.datamap == 0 && self.nodemap == 0 && self.leaves.is_empty() && self.children.is_empty()
    }

    /// Resolves which slot `path_hash` routes to at `depth` and returns what
    /// occupies it, if anything.
    ///
    /// This is the single shared CHAMP descent step: it computes the slot,
    /// tests it against `datamap`/`nodemap`, and resolves the compressed
    /// index. Both [`get_by_path_hash`](Self::get_by_path_hash) and
    /// [`search_by_path_hash`](Self::search_by_path_hash) build on it and
    /// only differ in how they terminate at a leaf or follow a child.
    fn slot_at(&self, path_hash: &StructuralHash, depth: usize) -> Slot<'_, K, V> {
        let slot = bucket_index(path_hash, depth);
        let bit = 1_u32 << slot;

        if (self.datamap & bit) != 0 {
            Slot::Leaf(&self.leaves[map_index(self.datamap, slot)])
        } else if (self.nodemap & bit) != 0 {
            Slot::Child(&self.children[map_index(self.nodemap, slot)])
        } else {
            Slot::Empty
        }
    }

    fn get_by_path_hash_inner<Q>(
        &self,
        key: &Q,
        path_hash: &StructuralHash,
        depth: usize,
    ) -> Option<&V>
    where
        K: Eq + Borrow<Q>,
        Q: Eq + ?Sized,
    {
        if self.shape_error().is_some() {
            return None;
        }
        if self.is_collision() {
            if depth != HAMT_MAX_DEPTH {
                return None;
            }
            return self
                .leaves
                .iter()
                .find(|(stored_key, _)| stored_key.borrow() == key)
                .map(|(_, value)| value);
        }
        if depth >= HAMT_MAX_DEPTH {
            return None;
        }
        let next_depth = depth.saturating_add(1);

        match self.slot_at(path_hash, depth) {
            Slot::Leaf((stored_key, value)) => (stored_key.borrow() == key).then_some(value),
            Slot::Child(NodeRef::Resolved(child)) => {
                child.get_by_path_hash_inner(key, path_hash, next_depth)
            }
            Slot::Child(NodeRef::Lazy(_)) | Slot::Empty => None,
        }
    }
}

/// What occupies a single CHAMP slot: a leaf entry, a child node, or nothing.
enum Slot<'a, K, V> {
    Leaf(&'a (K, V)),
    Child(&'a NodeRef<K, V>),
    Empty,
}

/// Returns the shared route for a terminal collision bucket, rejecting a
/// bucket whose keys do not all consume the same complete path hash.
pub(crate) fn collision_route_hash<K, V, F>(
    leaves: &[(K, V)],
    mut route: F,
) -> Result<StructuralHash, &'static str>
where
    F: FnMut(&K) -> StructuralHash,
{
    let Some((first_key, _)) = leaves.first() else {
        return Err("collision bucket is empty");
    };
    let hash = route(first_key);
    if leaves.iter().skip(1).any(|(key, _)| route(key) != hash) {
        return Err("leaf routed to wrong slot");
    }
    Ok(hash)
}

/// The routing hash of `key`: a keyed hash of its [`HamtRoutingKey`] bytes.
///
/// Every default-routed operation (`build_hamt`, `insert`, `remove`,
/// `persist_mutation(s)`, `get`, `search`, and the persisted reader) uses this,
/// so a tree routes by the persisted encoding of its keys and is byte-identical
/// across processes, versions and architectures.
#[must_use]
pub fn key_path_hash<K: HamtRoutingKey + ?Sized>(structural_key: &[u8], key: &K) -> StructuralHash {
    let mut encoded = Vec::new();
    key.encode_routing(&mut encoded);
    let mut hasher = StructuralHashBuilder::new(structural_key);
    hasher.update(&encoded);
    hasher.finalize()
}

pub(super) fn map_index(bitmap: u32, slot: usize) -> usize {
    (bitmap & (1_u32 << slot).wrapping_sub(1)).count_ones() as usize
}

fn bucket_index(hash: &StructuralHash, depth: usize) -> usize {
    debug_assert!(
        depth < HAMT_MAX_DEPTH,
        "bucket_index called at or beyond HAMT_MAX_DEPTH ({depth} >= {HAMT_MAX_DEPTH})"
    );
    let bit_offset = depth.saturating_mul(HAMT_BRANCH_BITS);
    let byte_index = bit_offset / 8;
    let bit_shift = bit_offset % 8;
    let hash_len = hash.len();
    debug_assert!(
        byte_index < hash_len,
        "byte_index out of bounds for StructuralHash ({byte_index} >= {hash_len})",
    );

    let mut word = u16::from(hash[byte_index]);
    // No checked_add: byte_index < hash_len (32, asserted above) always, so
    // byte_index + 1 <= 32 never overflows usize. A checked_add here can
    // never observe None -- it's not a real safety margin, just an untestable
    // dead branch (a coverage tool will flag its closing brace as unreached,
    // correctly: reaching it would require byte_index near usize::MAX, which
    // contradicts the precondition this function already asserts).
    if let Some(next) = hash.get(byte_index.saturating_add(1)) {
        word |= u16::from(*next) << 8;
    }
    usize::from((word >> bit_shift) & HAMT_BRANCH_MASK)
}

struct BuildEntry<K, V> {
    key: K,
    value: V,
    path_hash: StructuralHash,
}

/// Sorts a collision bucket into its canonical wire order (encoded key bytes).
fn sort_collision_leaves<K: HamtCodec, V>(leaves: &mut [(K, V)]) {
    leaves.sort_by_cached_key(|(key, _)| {
        let mut bytes = Vec::new();
        key.encode_hamt(&mut bytes);
        bytes
    });
}

fn build_node<K, V>(
    structural_key: &[u8],
    entries: Vec<BuildEntry<K, V>>,
    depth: usize,
) -> NodePtr<K, V>
where
    K: Eq + HamtCodec,
    V: HamtCodec,
{
    if depth >= HAMT_MAX_DEPTH {
        let mut leaves = entries
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect::<Vec<_>>();
        sort_collision_leaves(&mut leaves);
        let structural_hash = HamtNode::compute_structural_hash(structural_key, 0, 0, &leaves, &[]);
        return Arc::new(HamtNode {
            datamap: 0,
            nodemap: 0,
            leaves,
            children: Vec::new(),
            structural_hash,
        });
    }

    let mut buckets: Vec<Vec<BuildEntry<K, V>>> =
        (0..HAMT_BRANCH_FACTOR).map(|_| Vec::new()).collect();
    for entry in entries {
        let slot = bucket_index(&entry.path_hash, depth);
        buckets[slot].push(entry);
    }

    let mut datamap = 0_u32;
    let mut nodemap = 0_u32;
    let mut leaves = Vec::new();
    let mut children = Vec::new();

    for (slot, mut bucket) in buckets.into_iter().enumerate() {
        let Some(entry) = bucket.pop() else {
            continue;
        };
        let bit = 1_u32 << slot;
        if bucket.is_empty() {
            datamap |= bit;
            leaves.push((entry.key, entry.value));
            continue;
        }
        bucket.push(entry);
        nodemap |= bit;
        children.push(NodeRef::Resolved(build_node(
            structural_key,
            bucket,
            depth.saturating_add(1),
        )));
    }

    let structural_hash =
        HamtNode::compute_structural_hash(structural_key, datamap, nodemap, &leaves, &children);
    Arc::new(HamtNode {
        datamap,
        nodemap,
        leaves,
        children,
        structural_hash,
    })
}

/// Builds a full HAMT from an iterator of key/value entries.
///
/// The caller supplies the `structural_key` (typically the room's `room_id.as_bytes()`)
/// used for subtree hashing and deterministic routing within this namespace.
pub fn build_hamt<K, V, I>(structural_key: &[u8], entries: I) -> NodePtr<K, V>
where
    K: Eq + HamtCodec,
    V: HamtCodec,
    I: IntoIterator<Item = (K, V)>,
{
    build_entries(structural_key, entries, |key| {
        key_path_hash(structural_key, key)
    })
}

/// Collects entries into a deduplicated [`BuildEntry`] list. Duplicate keys
/// follow ordinary map semantics: the last value wins.
fn build_entries<K, V, I, F>(structural_key: &[u8], entries: I, mut key_hash: F) -> NodePtr<K, V>
where
    K: Eq + HamtCodec,
    V: HamtCodec,
    I: IntoIterator<Item = (K, V)>,
    F: FnMut(&K) -> StructuralHash,
{
    let mut all = entries
        .into_iter()
        .map(|(key, value)| BuildEntry {
            path_hash: key_hash(&key),
            key,
            value,
        })
        .collect::<Vec<_>>();

    // Equal keys always share a path hash, so a stable sort groups every
    // duplicate run while preserving insertion order. That keeps the dedup
    // work proportional to the number of actual duplicates — almost always
    // zero or one — instead of scanning the whole input for every entry.
    all.sort_by(|left, right| left.path_hash.cmp(&right.path_hash));

    let mut unique = Vec::with_capacity(all.len());
    let mut iter = all.into_iter().peekable();
    while let Some(first) = iter.next() {
        let run_hash = first.path_hash;
        // The overwhelmingly common case: a hash that occurs exactly once
        // (every non-duplicate key) needs no equality checks at all.
        if iter.peek().is_none_or(|next| next.path_hash != run_hash) {
            unique.push(first);
            continue;
        }

        // A run can only be large when duplicate keys share a hash, or when
        // distinct keys genuinely collide on all 256 bits.
        let mut run = vec![first];
        while let Some(entry) = iter.next_if(|next| next.path_hash == run_hash) {
            run.push(entry);
        }
        let mut deduped: Vec<BuildEntry<K, V>> = Vec::with_capacity(run.len());
        for entry in run {
            if let Some(old) = deduped.iter_mut().find(|old| old.key == entry.key) {
                // Keep the later key representation and value without shifting
                // the run's vector. Stable hash sorting preserves input order.
                old.key = entry.key;
                old.value = entry.value;
            } else {
                deduped.push(entry);
            }
        }
        unique.append(&mut deduped);
    }

    build_node(structural_key, unique, 0)
}

/// Builds a HAMT using a caller-supplied routing hash.
///
/// Only tests need this hook; production trees route with [`key_path_hash`].
#[cfg(test)]
fn build_hamt_with_routing<K, V, I, F>(
    structural_key: &[u8],
    entries: I,
    key_hash: F,
) -> NodePtr<K, V>
where
    K: Eq + HamtCodec,
    V: HamtCodec,
    I: IntoIterator<Item = (K, V)>,
    F: FnMut(&K) -> StructuralHash,
{
    build_entries(structural_key, entries, key_hash)
}

/// Builds a root handle for a freshly constructed HAMT.
pub fn build_hamt_root_handle<K, V, I>(
    structural_key: &[u8],
    lattice: &crate::incremental::LtHash,
    entries: I,
) -> (RootHandle, NodePtr<K, V>)
where
    K: Eq + HamtCodec,
    V: HamtCodec,
    I: IntoIterator<Item = (K, V)>,
{
    let root = build_hamt(structural_key, entries);
    let handle = RootHandle::from_lthash(root.structural_hash, lattice);
    (handle, root)
}

/// Errors that can occur while incrementally mutating a HAMT via
/// [`insert`] or [`remove`].
#[derive(Debug, PartialEq, Eq)]
pub enum HamtMutateError<E> {
    /// Traversal exceeded maximum allowable HAMT depth.
    MaxDepthExceeded { depth: usize },
    /// A node's shape is illegal for the depth it occupies (for example, a
    /// collision bucket above the routing limit). This can come from malformed
    /// persisted data, so it is an error rather than a panic. Note that the
    /// read-only lookup APIs have no error channel and report such a shape as
    /// absent instead.
    CorruptNode(&'static str),
    /// The resolver failed to load a lazy child that the mutation needed to
    /// descend into.
    Resolve(E),
}

impl<E> fmt::Display for HamtMutateError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaxDepthExceeded { depth } => {
                write!(f, "hamt mutation max depth exceeded at depth {depth}")
            }
            Self::CorruptNode(reason) => write!(f, "hamt mutation corrupt node: {reason}"),
            Self::Resolve(err) => write!(f, "hamt mutation resolver failed: {err}"),
        }
    }
}

impl<E> core::error::Error for HamtMutateError<E>
where
    E: core::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::MaxDepthExceeded { .. } | Self::CorruptNode(_) => None,
            Self::Resolve(err) => Some(err),
        }
    }
}

impl<E> From<HamtTraversalError<E>> for HamtMutateError<E> {
    fn from(err: HamtTraversalError<E>) -> Self {
        match err {
            HamtTraversalError::Resolve(e) => Self::Resolve(e),
            HamtTraversalError::CorruptNode(reason) => Self::CorruptNode(reason),
            HamtTraversalError::MaxDepthExceeded { depth } => Self::MaxDepthExceeded { depth },
        }
    }
}

/// Result of an [`insert`]/`insert_node` mutation: the new root and the
/// value that previously occupied the key, if any.
type MutateResult<K, V, E> = Result<(NodePtr<K, V>, Option<V>), HamtMutateError<E>>;

/// Result of a `remove_node` step: what remains of the subtree, and the
/// value that was removed, if the key was present.
type RemoveStepResult<K, V, E> = Result<(RemoveOutcome<K, V>, Option<V>), HamtMutateError<E>>;

fn rebuild_node<K, V>(
    structural_key: &[u8],
    datamap: u32,
    nodemap: u32,
    leaves: Vec<(K, V)>,
    children: Vec<NodeRef<K, V>>,
) -> NodePtr<K, V>
where
    K: HamtCodec,
    V: HamtCodec,
{
    let structural_hash =
        HamtNode::compute_structural_hash(structural_key, datamap, nodemap, &leaves, &children);
    Arc::new(HamtNode {
        datamap,
        nodemap,
        leaves,
        children,
        structural_hash,
    })
}

type MutationSink<'a, K, V> = dyn FnMut(&NodePtr<K, V>) + 'a;

fn emit_node_to_sink<K, V>(node: &NodePtr<K, V>, sink: &mut MutationSink<'_, K, V>) {
    sink(node);
}

fn emit_split_nodes_to_sink<K, V>(node: &NodePtr<K, V>, sink: &mut MutationSink<'_, K, V>) {
    sink(node);
    for child in &node.children {
        if let NodeRef::Resolved(child_node) = child {
            emit_split_nodes_to_sink(child_node, sink);
        }
    }
}

fn resolve_child_ref<K, V, F, E>(
    child: &NodeRef<K, V>,
    resolver: &mut F,
) -> Result<NodePtr<K, V>, HamtMutateError<E>>
where
    F: NodeResolver<K, V, E>,
{
    match child {
        NodeRef::Resolved(child) => Ok(child.clone()),
        NodeRef::Lazy(hash) => resolver(hash).map_err(HamtMutateError::Resolve),
    }
}

fn check_mutation_depth<K, V, E>(
    node: &HamtNode<K, V>,
    depth: usize,
) -> Result<(), HamtMutateError<E>> {
    if let Some(reason) = node.shape_error() {
        return Err(HamtMutateError::CorruptNode(reason));
    }
    if depth > HAMT_MAX_DEPTH {
        return Err(HamtMutateError::MaxDepthExceeded { depth });
    }
    if node.is_collision() && depth != HAMT_MAX_DEPTH {
        return Err(HamtMutateError::CorruptNode("misplaced collision bucket"));
    }
    Ok(())
}

fn rebuild_with_child<K, V>(
    node: &NodePtr<K, V>,
    structural_key: &[u8],
    idx: usize,
    new_child: NodePtr<K, V>,
    sink: &mut MutationSink<'_, K, V>,
) -> NodePtr<K, V>
where
    K: Clone + HamtCodec,
    V: HamtValue,
{
    let mut children = node.children.clone();
    children[idx] = NodeRef::Resolved(new_child);
    let new_node = rebuild_node(
        structural_key,
        node.datamap,
        node.nodemap,
        node.leaves.clone(),
        children,
    );
    emit_node_to_sink(&new_node, sink);
    new_node
}

#[cfg(test)]
fn insert_node<K, V, F, E>(
    node: &NodePtr<K, V>,
    structural_key: &[u8],
    key: K,
    value: V,
    path_hash: &StructuralHash,
    depth: usize,
    resolver: &mut F,
) -> MutateResult<K, V, E>
where
    K: HamtKey,
    V: HamtValue,
    F: NodeResolver<K, V, E>,
{
    let mut key_hash = |key: &K| key_path_hash(structural_key, key);
    let mut sink = |_: &NodePtr<K, V>| {};
    insert_via_ctx(
        node,
        structural_key,
        key,
        value,
        *path_hash,
        depth,
        &mut key_hash,
        resolver,
        &mut sink,
    )
}

#[allow(clippy::too_many_arguments)]
fn insert_via_ctx<K, V, KeyHash, F, E>(
    node: &NodePtr<K, V>,
    structural_key: &[u8],
    key: K,
    value: V,
    path_hash: StructuralHash,
    depth: usize,
    key_hash: &mut KeyHash,
    resolver: &mut F,
    sink: &mut MutationSink<'_, K, V>,
) -> MutateResult<K, V, E>
where
    K: HamtKey,
    V: HamtValue,
    KeyHash: FnMut(&K) -> StructuralHash,
    F: NodeResolver<K, V, E>,
{
    let mut ctx = InsertCtx {
        structural_key,
        key_hash,
        resolver,
        sink,
    };
    ctx.insert_node(node, key, value, path_hash, depth)
}

struct InsertCtx<'a, K, V, KeyHash, F> {
    structural_key: &'a [u8],
    key_hash: &'a mut KeyHash,
    resolver: &'a mut F,
    sink: &'a mut MutationSink<'a, K, V>,
}

struct InsertStep<K, V> {
    key: K,
    value: V,
    path_hash: StructuralHash,
    slot: usize,
    bit: u32,
}

impl<K, V, KeyHash, F> InsertCtx<'_, K, V, KeyHash, F>
where
    K: HamtKey,
    V: HamtValue,
    KeyHash: FnMut(&K) -> StructuralHash,
{
    fn insert_node<E>(
        &mut self,
        node: &NodePtr<K, V>,
        key: K,
        value: V,
        path_hash: StructuralHash,
        depth: usize,
    ) -> MutateResult<K, V, E>
    where
        F: NodeResolver<K, V, E>,
    {
        check_mutation_depth(node, depth)?;
        if node.is_collision() {
            let old_value = node
                .leaves
                .iter()
                .find(|(old_key, _)| old_key == &key)
                .map(|(_, value)| value.clone());
            let mut leaves = node
                .leaves
                .iter()
                .filter(|(old_key, _)| old_key != &key)
                .map(|(old_key, old_value)| (old_key.clone(), old_value.clone()))
                .collect::<Vec<_>>();
            leaves.push((key, value));
            sort_collision_leaves(&mut leaves);
            let new_node = rebuild_node(self.structural_key, 0, 0, leaves, Vec::new());
            emit_node_to_sink(&new_node, self.sink);
            return Ok((new_node, old_value));
        }
        if depth >= HAMT_MAX_DEPTH {
            return Err(HamtMutateError::MaxDepthExceeded { depth });
        }

        let structural_key = self.structural_key;
        let slot = bucket_index(&path_hash, depth);
        let bit = 1_u32 << slot;
        let step = InsertStep {
            key,
            value,
            path_hash,
            slot,
            bit,
        };

        if (node.datamap & bit) != 0 {
            let idx = map_index(node.datamap, slot);
            if node.leaves[idx].0 == step.key {
                let mut leaves = node.leaves.clone();
                let old_value = core::mem::replace(&mut leaves[idx].1, step.value);
                let new_node = rebuild_node(
                    structural_key,
                    node.datamap,
                    node.nodemap,
                    leaves,
                    node.children.clone(),
                );
                emit_node_to_sink(&new_node, self.sink);
                return Ok((new_node, Some(old_value)));
            }

            return self.insert_into_occupied_slot(node, step, depth.saturating_add(1));
        }

        if (node.nodemap & bit) != 0 {
            return self.insert_into_occupied_slot(node, step, depth.saturating_add(1));
        }

        // Empty slot: insert directly as a new leaf.
        let mut leaves = node.leaves.clone();
        let idx = map_index(node.datamap, step.slot);
        leaves.insert(idx, (step.key, step.value));
        let new_datamap = node.datamap | step.bit;
        let new_node = rebuild_node(
            structural_key,
            new_datamap,
            node.nodemap,
            leaves,
            node.children.clone(),
        );
        emit_node_to_sink(&new_node, self.sink);
        Ok((new_node, None))
    }

    fn insert_into_occupied_slot<E>(
        &mut self,
        node: &NodePtr<K, V>,
        step: InsertStep<K, V>,
        next_depth: usize,
    ) -> MutateResult<K, V, E>
    where
        F: NodeResolver<K, V, E>,
    {
        if (node.datamap & step.bit) != 0 {
            // Leaf slot: split the displaced leaf and this one into a new child.
            let InsertStep {
                key,
                value,
                path_hash,
                slot,
                bit,
            } = step;
            let idx = map_index(node.datamap, slot);

            let (existing_key, existing_value) = node.leaves[idx].clone();
            let existing_path_hash = (self.key_hash)(&existing_key);
            let split_entries = vec![
                BuildEntry {
                    key: existing_key,
                    value: existing_value,
                    path_hash: existing_path_hash,
                },
                BuildEntry {
                    key,
                    value,
                    path_hash,
                },
            ];
            let child = build_node(self.structural_key, split_entries, next_depth);
            emit_split_nodes_to_sink(&child, self.sink);

            let mut leaves = node.leaves.clone();
            leaves.remove(idx);
            let new_datamap = node.datamap & !bit;
            let new_nodemap = node.nodemap | bit;
            let child_idx = map_index(new_nodemap, slot);
            let mut children = node.children.clone();
            children.insert(child_idx, NodeRef::Resolved(child));
            let new_node = rebuild_node(
                self.structural_key,
                new_datamap,
                new_nodemap,
                leaves,
                children,
            );
            emit_node_to_sink(&new_node, self.sink);
            Ok((new_node, None))
        } else {
            // Child slot: recurse into the existing child.
            let InsertStep {
                key,
                value,
                path_hash,
                slot,
                ..
            } = step;
            let idx = map_index(node.nodemap, slot);
            let child = resolve_child_ref(&node.children[idx], self.resolver)?;
            let (new_child, old_value) =
                self.insert_node(&child, key, value, path_hash, next_depth)?;
            let new_node = rebuild_with_child(node, self.structural_key, idx, new_child, self.sink);
            Ok((new_node, old_value))
        }
    }
}

/// Inserts or replaces a key/value pair in a HAMT via `O(log S)`
/// path-copying, without rebuilding the whole tree.
///
/// Returns the new root and the value that previously occupied `key`, if
/// any — callers that maintain a homomorphic lattice alongside the tree
/// (e.g. `LtHash`) need the displaced value to subtract it before adding
/// the new one.
///
/// # Errors
/// Returns [`HamtMutateError::Resolve`] if `resolver` fails to load a lazy
/// child on the path to `key`, or `MaxDepthExceeded` for malformed trees.
pub fn insert<K, V, F, E>(
    node: &NodePtr<K, V>,
    structural_key: &[u8],
    key: K,
    value: V,
    resolver: &mut F,
) -> MutateResult<K, V, E>
where
    K: HamtKey,
    V: HamtValue,
    F: NodeResolver<K, V, E>,
{
    let path_hash = key_path_hash(structural_key, &key);
    let mut sink = |_: &NodePtr<K, V>| {};
    let mut key_hash = |key: &K| key_path_hash(structural_key, key);
    insert_via_ctx(
        node,
        structural_key,
        key,
        value,
        path_hash,
        0,
        &mut key_hash,
        resolver,
        &mut sink,
    )
}

/// Test-only insert with a caller-supplied routing hash. Production trees
/// route with [`key_path_hash`] only, so this exists purely to force
/// collisions in unit tests.
#[cfg(test)]
fn insert_with_routing<K, V, KeyHash, F, E>(
    node: &NodePtr<K, V>,
    structural_key: &[u8],
    key: K,
    value: V,
    key_hash: &mut KeyHash,
    resolver: &mut F,
) -> MutateResult<K, V, E>
where
    K: HamtKey,
    V: HamtValue,
    KeyHash: FnMut(&K) -> StructuralHash,
    F: NodeResolver<K, V, E>,
{
    let path_hash = key_hash(&key);
    let mut sink = |_: &NodePtr<K, V>| {};
    insert_via_ctx(
        node,
        structural_key,
        key,
        value,
        path_hash,
        0,
        key_hash,
        resolver,
        &mut sink,
    )
}

/// What remains of a subtree after a leaf was removed from it.
#[derive(Debug)]
enum RemoveOutcome<K, V> {
    /// The subtree has no entries left.
    Empty,
    /// The subtree collapsed to a single leaf, which must be inlined into
    /// the parent's `datamap` rather than kept as a child node — the same
    /// invariant `build_node` enforces when building from scratch.
    Leaf(K, V),
    /// The subtree still has two or more entries and remains a node.
    Node(NodePtr<K, V>),
}

/// Builds the outcome for a node whose contents just changed, collapsing it
/// to `Leaf`/`Empty` if it no longer has enough entries to justify being a
/// node, so the structural hash always matches what `build_node` would have
/// produced for the same final key set.
fn finalize_after_removal<K, V>(
    structural_key: &[u8],
    datamap: u32,
    nodemap: u32,
    leaves: Vec<(K, V)>,
    children: Vec<NodeRef<K, V>>,
) -> RemoveOutcome<K, V>
where
    K: HamtCodec,
    V: HamtCodec,
{
    if nodemap == 0 {
        if leaves.is_empty() {
            return RemoveOutcome::Empty;
        }
        if leaves.len() == 1 {
            let (key, value) = leaves
                .into_iter()
                .next()
                .expect("checked leaves.len() == 1 above");
            return RemoveOutcome::Leaf(key, value);
        }
    }
    RemoveOutcome::Node(rebuild_node(
        structural_key,
        datamap,
        nodemap,
        leaves,
        children,
    ))
}

struct RemoveCtx<'a, K, V, F> {
    structural_key: &'a [u8],
    resolver: &'a mut F,
    sink: &'a mut MutationSink<'a, K, V>,
}

fn remove_spine<K, V, Q, F, E>(
    node: &NodePtr<K, V>,
    structural_key: &[u8],
    key: &Q,
    path_hash: &StructuralHash,
    leaf_root_slot: impl FnOnce(&K) -> usize,
    resolver: &mut F,
    sink: &mut MutationSink<'_, K, V>,
) -> MutateResult<K, V, E>
where
    K: Eq + Borrow<Q> + Clone + HamtCodec,
    V: HamtValue,
    Q: Eq + ?Sized,
    F: NodeResolver<K, V, E>,
{
    let (outcome, old_value) = {
        let mut ctx = RemoveCtx {
            structural_key,
            resolver,
            sink,
        };
        remove_node_with_ctx(node, key, path_hash, 0, &mut ctx)?
    };
    let new_root = finalize_remove_root(structural_key, outcome, leaf_root_slot, sink);
    Ok((new_root, old_value))
}

/// Removes a key from a HAMT via `O(log S)` path-copying, without
/// rebuilding the whole tree.
///
/// Collapses any subtree that drops to a single leaf back into its
/// parent's `datamap`, cascading as needed, so the resulting tree's
/// `structural_hash` always matches what [`build_hamt`] would have
/// produced from the same final key set.
///
/// Returns the new root and the removed value, if `key` was present.
///
/// # Errors
/// Returns [`HamtMutateError::Resolve`] if `resolver` fails to load a lazy
/// child on the path to `key`.
pub fn remove<K, V, Q, F, E>(
    node: &NodePtr<K, V>,
    structural_key: &[u8],
    key: &Q,
    resolver: &mut F,
) -> MutateResult<K, V, E>
where
    K: Eq + Borrow<Q> + Clone + HamtCodec,
    V: HamtValue,
    Q: HamtRoutingKey + Eq + ?Sized,
    F: NodeResolver<K, V, E>,
{
    let path_hash = key_path_hash(structural_key, key);
    let mut sink = |_: &NodePtr<K, V>| {};
    remove_spine(
        node,
        structural_key,
        key,
        &path_hash,
        |leaf_key| bucket_index(&key_path_hash(structural_key, leaf_key), 0),
        resolver,
        &mut sink,
    )
}

/// Test-only remove with a caller-supplied routing hash. Production trees
/// route with [`key_path_hash`] only.
#[cfg(test)]
fn remove_with_routing<K, V, Q, KeyHash, F, E>(
    node: &NodePtr<K, V>,
    structural_key: &[u8],
    key: &Q,
    key_hash: &mut KeyHash,
    resolver: &mut F,
) -> MutateResult<K, V, E>
where
    K: Eq + Borrow<Q> + Clone + HamtCodec,
    V: HamtValue,
    Q: Eq + Borrow<K> + ?Sized,
    KeyHash: FnMut(&K) -> StructuralHash,
    F: NodeResolver<K, V, E>,
{
    let path_hash = key_hash(key.borrow());
    let mut sink = |_: &NodePtr<K, V>| {};
    remove_spine(
        node,
        structural_key,
        key,
        &path_hash,
        |leaf_key| bucket_index(&key_hash(leaf_key), 0),
        resolver,
        &mut sink,
    )
}

#[allow(clippy::too_many_lines)] // one linear descent with a collision special case
fn remove_node_with_ctx<K, V, Q, F, E>(
    node: &NodePtr<K, V>,
    key: &Q,
    path_hash: &StructuralHash,
    depth: usize,
    ctx: &mut RemoveCtx<'_, K, V, F>,
) -> RemoveStepResult<K, V, E>
where
    K: Eq + Borrow<Q> + Clone + HamtCodec,
    V: HamtValue,
    Q: Eq + ?Sized,
    F: NodeResolver<K, V, E>,
{
    check_mutation_depth(node, depth)?;
    if node.is_collision() {
        let Some(index) = node
            .leaves
            .iter()
            .position(|(stored, _)| stored.borrow() == key)
        else {
            return Ok((RemoveOutcome::Node(node.clone()), None));
        };
        let old_value = node.leaves[index].1.clone();
        let mut leaves = node.leaves.clone();
        leaves.remove(index);
        let outcome = match leaves.len() {
            0 => RemoveOutcome::Empty,
            1 => {
                let (key, value) = leaves.pop().expect("checked len == 1");
                RemoveOutcome::Leaf(key, value)
            }
            _ => {
                let new_node = rebuild_node(ctx.structural_key, 0, 0, leaves, Vec::new());
                emit_node_to_sink(&new_node, ctx.sink);
                RemoveOutcome::Node(new_node)
            }
        };
        return Ok((outcome, Some(old_value)));
    }
    if depth >= HAMT_MAX_DEPTH {
        return Err(HamtMutateError::MaxDepthExceeded { depth });
    }

    let slot = bucket_index(path_hash, depth);
    let bit = 1_u32 << slot;
    let structural_key = ctx.structural_key;

    if (node.datamap & bit) != 0 {
        let idx = map_index(node.datamap, slot);
        if node.leaves[idx].0.borrow() != key {
            return Ok((RemoveOutcome::Node(node.clone()), None));
        }

        let old_value = node.leaves[idx].1.clone();
        let mut leaves = node.leaves.clone();
        leaves.remove(idx);
        let new_datamap = node.datamap & !bit;
        let outcome = finalize_after_removal(
            structural_key,
            new_datamap,
            node.nodemap,
            leaves,
            node.children.clone(),
        );
        if let RemoveOutcome::Node(ref new_node) = outcome {
            emit_node_to_sink(new_node, ctx.sink);
        }
        return Ok((outcome, Some(old_value)));
    }

    if (node.nodemap & bit) == 0 {
        return Ok((RemoveOutcome::Node(node.clone()), None));
    }

    let idx = map_index(node.nodemap, slot);
    let child = resolve_child_ref(&node.children[idx], ctx.resolver)?;

    let next_depth = depth.saturating_add(1);
    let (child_outcome, old_value) = remove_node_with_ctx(&child, key, path_hash, next_depth, ctx)?;
    if old_value.is_none() {
        return Ok((RemoveOutcome::Node(node.clone()), None));
    }

    match child_outcome {
        RemoveOutcome::Empty => {
            let mut children = node.children.clone();
            children.remove(idx);
            let new_nodemap = node.nodemap & !bit;
            let outcome = finalize_after_removal(
                structural_key,
                node.datamap,
                new_nodemap,
                node.leaves.clone(),
                children,
            );
            if let RemoveOutcome::Node(ref new_node) = outcome {
                emit_node_to_sink(new_node, ctx.sink);
            }
            Ok((outcome, old_value))
        }
        RemoveOutcome::Leaf(leaf_key, leaf_value) => {
            let mut children = node.children.clone();
            children.remove(idx);
            let new_nodemap = node.nodemap & !bit;
            let new_datamap = node.datamap | bit;
            let leaf_idx = map_index(node.datamap, slot);
            let mut leaves = node.leaves.clone();
            leaves.insert(leaf_idx, (leaf_key, leaf_value));
            let outcome =
                finalize_after_removal(structural_key, new_datamap, new_nodemap, leaves, children);
            if let RemoveOutcome::Node(ref new_node) = outcome {
                emit_node_to_sink(new_node, ctx.sink);
            }
            Ok((outcome, old_value))
        }
        RemoveOutcome::Node(new_child) => {
            let new_node = rebuild_with_child(node, structural_key, idx, new_child, ctx.sink);
            Ok((RemoveOutcome::Node(new_node), old_value))
        }
    }
}

fn finalize_remove_root<K, V>(
    structural_key: &[u8],
    outcome: RemoveOutcome<K, V>,
    leaf_root_slot: impl FnOnce(&K) -> usize,
    sink: &mut MutationSink<'_, K, V>,
) -> NodePtr<K, V>
where
    K: Clone + HamtCodec,
    V: HamtValue,
{
    match outcome {
        RemoveOutcome::Empty => {
            let root = rebuild_node(structural_key, 0, 0, Vec::new(), Vec::new());
            emit_node_to_sink(&root, sink);
            root
        }
        RemoveOutcome::Leaf(leaf_key, leaf_value) => {
            let root_slot = leaf_root_slot(&leaf_key);
            let leaves = vec![(leaf_key, leaf_value)];
            let root = rebuild_node(structural_key, 1_u32 << root_slot, 0, leaves, Vec::new());
            emit_node_to_sink(&root, sink);
            root
        }
        RemoveOutcome::Node(new_root) => new_root,
    }
}

/// Result of a [`persist_mutation`] operation: the new root, displaced value, and encoded new nodes.
pub type PersistMutationResult<K, V, E> =
    Result<(NodePtr<K, V>, Option<V>, Vec<(StructuralHash, Vec<u8>)>), HamtMutateError<E>>;

/// Result of a [`persist_mutations`] batch operation: the new root, displaced values, and encoded new nodes.
pub type PersistMutationsResult<K, V, E> = Result<
    (
        NodePtr<K, V>,
        Vec<Option<V>>,
        Vec<(StructuralHash, Vec<u8>)>,
    ),
    HamtMutateError<E>,
>;

/// Persists a single mutation (insert if `Some`, remove if `None`) on top of `prev_root`.
///
/// Emits all created spine nodes inline during the mutation descent without a second
/// post-hoc tree traversal or redundant lazy-child resolutions.
///
/// Returns:
/// - The new root `NodePtr<K, V>`
/// - The displaced / removed value, if any (needed for lattice subtraction)
/// - The encoded binary bytes of the newly created spine nodes (`Vec<(StructuralHash, Vec<u8>)>`)
///
/// # Publication Contract
/// This is for a single published state group. The returned `created` nodes must be written to
/// storage before publishing the state group root pointer.
///
/// # Errors
/// Returns [`HamtMutateError`] if `resolver` fails to load a lazy child or depth is exhausted.
pub fn persist_mutation<K, V, F, E>(
    prev_root: &NodePtr<K, V>,
    structural_key: &[u8],
    key: K,
    value: Option<V>,
    resolver: &mut F,
) -> PersistMutationResult<K, V, E>
where
    K: HamtKey,
    V: HamtValue,
    F: NodeResolver<K, V, E>,
{
    let path_hash = key_path_hash(structural_key, &key);
    let mut created = Vec::new();
    let (new_root, displaced) = {
        let mut sink = |node: &NodePtr<K, V>| {
            created.push((
                node.structural_hash,
                PersistedInternalNode::from(node.as_ref()).encode_v1(),
            ));
        };
        if let Some(val) = value {
            let mut key_hash = |leaf_key: &K| key_path_hash(structural_key, leaf_key);
            insert_via_ctx(
                prev_root,
                structural_key,
                key,
                val,
                path_hash,
                0,
                &mut key_hash,
                resolver,
                &mut sink,
            )?
        } else {
            remove_spine(
                prev_root,
                structural_key,
                &key,
                &path_hash,
                |leaf_key| bucket_index(&key_path_hash(structural_key, leaf_key), 0),
                resolver,
                &mut sink,
            )?
        }
    };

    if new_root.structural_hash == prev_root.structural_hash {
        created.clear();
    }

    Ok((new_root, displaced, created))
}

fn finalize_persisted_mutations<K, V, F, E>(
    prev_root: &NodePtr<K, V>,
    current_root: NodePtr<K, V>,
    displaced_vec: Vec<Option<V>>,
    resolver: &mut F,
) -> PersistMutationsResult<K, V, E>
where
    K: HamtKey,
    V: HamtValue,
    F: NodeResolver<K, V, E>,
{
    if current_root.structural_hash == prev_root.structural_hash {
        return Ok((current_root, displaced_vec, Vec::new()));
    }

    let delta = diff_node_hashes(prev_root, &current_root, resolver)?;
    let new_set: crate::HashSet<StructuralHash> = delta.new_node_hashes.into_iter().collect();

    let mut created = Vec::with_capacity(new_set.len());
    let mut stack = vec![current_root.clone()];
    while let Some(node) = stack.pop() {
        if new_set.contains(&node.structural_hash) {
            let bytes = PersistedInternalNode::from(node.as_ref()).encode_v1();
            created.push((node.structural_hash, bytes));
            for child in &node.children {
                if let NodeRef::Resolved(child_node) = child {
                    if new_set.contains(&child_node.structural_hash) {
                        stack.push(child_node.clone());
                    }
                }
            }
        }
    }

    Ok((current_root, displaced_vec, created))
}

/// Persists a batch of mutations producing a SINGLE published final root.
///
/// # Publication Contract — READ BEFORE USE
/// This function is ONLY correct when **no intermediate roots are published** (e.g.
/// bulk initial state build or a single coalesced update). If any intermediate state group
/// is published or referenced elsewhere, you MUST use [`persist_chain`] instead; otherwise,
/// intermediate state groups will point to nodes that were never persisted.
///
/// Returns the final root, all displaced values per mutation (for lattice arithmetic),
/// and the encoded new nodes.
///
/// # Performance Note
/// Callers using lazy resolvers should pass a memoizing resolver (or use [`persist_chain`])
/// to avoid repeated backend fetches during the end-of-batch diff pass.
///
/// # Errors
/// Returns [`HamtMutateError`] if `resolver` fails to load a lazy child or depth is exhausted.
pub fn persist_mutations<K, V, I, F, E>(
    prev_root: &NodePtr<K, V>,
    structural_key: &[u8],
    mutations: I,
    resolver: &mut F,
) -> PersistMutationsResult<K, V, E>
where
    K: HamtKey,
    V: HamtValue,
    I: IntoIterator<Item = (K, Option<V>)>,
    F: NodeResolver<K, V, E>,
{
    let mut current_root = prev_root.clone();
    let mut displaced_vec = Vec::new();
    for (key, opt_val) in mutations {
        let (next_root, displaced) = if let Some(val) = opt_val {
            insert(&current_root, structural_key, key, val, resolver)?
        } else {
            remove(&current_root, structural_key, &key, resolver)?
        };
        displaced_vec.push(displaced);
        current_root = next_root;
    }
    finalize_persisted_mutations(prev_root, current_root, displaced_vec, resolver)
}

/// Persists a sequence of mutations where EVERY intermediate step publishes a distinct root.
///
/// This is the entry point used by homeserver batch-persistence paths (e.g. Synapse's
/// `store_state_deltas_for_batched`). It emits [`ChainStep`]s containing the exact set of
/// new nodes created at each step relative to its immediate predecessor.
///
/// # Publication Contract
/// For each step $i$, the `step.created` nodes must be written to storage before publishing
/// the corresponding state group root.
///
/// # Errors
/// Returns [`HamtMutateError`] if `resolver` fails to load a lazy child or depth is exhausted.
pub fn persist_chain<K, V, I, F, E>(
    prev_root: &NodePtr<K, V>,
    structural_key: &[u8],
    mutations: I,
    resolver: &mut F,
) -> Result<Vec<ChainStep<K, V>>, HamtMutateError<E>>
where
    K: HamtKey,
    V: HamtValue,
    I: IntoIterator<Item = (K, Option<V>)>,
    F: NodeResolver<K, V, E>,
{
    let mut steps = Vec::new();
    let mut current_root = prev_root.clone();
    for (key, opt_val) in mutations {
        let (next_root, displaced, created) =
            persist_mutation(&current_root, structural_key, key, opt_val, resolver)?;
        steps.push(ChainStep {
            root_hash: next_root.structural_hash,
            root: next_root.clone(),
            displaced,
            created,
        });
        current_root = next_root;
    }
    Ok(steps)
}

/// Descends into a batch of persisted node byte slices for their targeted keys.
///
/// `nodes_and_keys` contains the expected node hash, encoded bytes, depth, and
/// target key hashes for each fetched node. All entries in a single `nodes_and_keys`
/// call must share the same `depth` (the current level of breadth-first descent).
/// Each node is evaluated only against the keys that routed to it at that depth.
/// The default keyed path hash verifies exact leaf matches.
///
/// # Depth Contract
/// All tuples in `nodes_and_keys` must specify the same `depth` level. Successful
/// descent produces [`DescendResult::pending`] entries representing child nodes at `depth + 1`.
///
/// # Absence Invariant
/// A key is reported in `absent` ONLY when:
/// 1. Its routed slot bit is clear in both `datamap` and `nodemap`.
/// 2. Its routed slot bit is set in `datamap`, but the decoded leaf key at that slot has a
///    different path hash. At a terminal collision node, every leaf sharing the requested path
///    hash is returned; path hashes alone cannot distinguish a full 256-bit collision.
///
/// # Errors
/// Returns [`DescendError::Decode`] if any node byte slice fails v1 parsing, or
/// [`DescendError::CorruptNode`] if internal bitmap invariants are violated.
pub fn descend_level<K, V>(
    structural_key: &[u8],
    nodes_and_keys: &[(StructuralHash, &[u8], usize, &[KeyPathHash])],
) -> Result<DescendResult<K, V>, DescendError>
where
    K: HamtKey,
    V: HamtValue,
{
    descend_level_impl(structural_key, nodes_and_keys, |key| {
        key_path_hash(structural_key, key)
    })
}

/// Routing-parameterized implementation of [`descend_level`], so tests can
/// force terminal collision buckets with a synthetic routing hash.
fn descend_level_impl<K, V, F>(
    structural_key: &[u8],
    nodes_and_keys: &[(StructuralHash, &[u8], usize, &[KeyPathHash])],
    mut key_hash: F,
) -> Result<DescendResult<K, V>, DescendError>
where
    K: HamtKey,
    V: HamtValue,
    F: FnMut(&K) -> StructuralHash,
{
    if let Some(&(_, _, first_depth, _)) = nodes_and_keys.first() {
        // Runtime check: mixed-depth entries would merge requests from different
        // levels into one frontier, misrouting subsequent lookups. The
        // debug_assert! was previously only checked in debug builds.
        if !nodes_and_keys.iter().all(|&(_, _, d, _)| d == first_depth) {
            return Err(DescendError::CorruptNode(
                "mixed-depth entries in a single descend_level call",
            ));
        }
    }

    let mut found = Vec::new();
    let mut absent = Vec::new();
    let mut pending_map: crate::HashMap<StructuralHash, Vec<KeyPathHash>> =
        crate::HashMap::default();

    for &(expected_hash, node_bytes, depth, target_keys) in nodes_and_keys {
        let node = PersistedInternalNode::<K, V>::decode_v1_unverified(node_bytes)
            .map_err(DescendError::Decode)?
            .into_hamt_node_verified(structural_key, expected_hash)
            .map_err(|_| {
                DescendError::CorruptNode("decoded node contents do not match requested node hash")
            })?;
        if depth > HAMT_MAX_DEPTH || (depth == HAMT_MAX_DEPTH && !node.is_collision()) {
            return Err(DescendError::CorruptNode(
                "HAMT node shape is invalid at this depth",
            ));
        }
        if node.is_collision() && depth != HAMT_MAX_DEPTH {
            return Err(DescendError::CorruptNode("misplaced collision bucket"));
        }
        let collision_path_hash = if node.is_collision() {
            Some(
                collision_route_hash(&node.leaves, &mut key_hash)
                    .map_err(DescendError::CorruptNode)?,
            )
        } else {
            None
        };

        for &req_hash in target_keys {
            if node.is_collision() {
                if collision_path_hash == Some(req_hash) {
                    found.extend(
                        node.leaves
                            .iter()
                            .map(|(leaf_key, value)| (req_hash, leaf_key.clone(), value.clone())),
                    );
                } else {
                    absent.push(req_hash);
                }
                continue;
            }
            let slot = bucket_index(&req_hash, depth);
            let bit = 1_u32 << slot;

            if (node.datamap & bit) != 0 {
                let leaf_idx = map_index(node.datamap, slot);
                let (leaf_key, val) = node
                    .leaves
                    .get(leaf_idx)
                    .ok_or(DescendError::CorruptNode("leaf index out of bounds"))?;
                // Lazy slot validation: only check the leaf being read, not
                // the entire datamap. In a correct CHAMP HAMT, a leaf at
                // slot `s` must satisfy bucket_index(key_hash(leaf_key), depth) == s.
                let expected_slot = bucket_index(&key_hash(leaf_key), depth);
                if expected_slot != slot {
                    return Err(DescendError::CorruptNode("leaf routed to wrong slot"));
                }
                if key_hash(leaf_key) == req_hash {
                    found.push((req_hash, leaf_key.clone(), val.clone()));
                } else {
                    // Different leaf occupies this exact slot: key is proven absent
                    // The path hash matches but the leaf key may not; a MTHC terminal bucket
                    // is handled above because path hashes alone cannot distinguish its leaves.
                    absent.push(req_hash);
                }
            } else if (node.nodemap & bit) != 0 {
                let child_idx = map_index(node.nodemap, slot);
                let child_hash = node
                    .children
                    .get(child_idx)
                    .ok_or(DescendError::CorruptNode("child index out of bounds"))?
                    .structural_hash();
                pending_map.entry(child_hash).or_default().push(req_hash);
            } else {
                // Bitmap slot is clear: proven, terminal absence
                absent.push(req_hash);
            }
        }
    }

    let pending = pending_map.into_iter().collect();

    Ok(DescendResult {
        found,
        absent,
        pending,
    })
}
