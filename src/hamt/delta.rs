//! Subtree differencing and mutation delta tracking for HAMT tries.

use alloc::{sync::Arc, vec::Vec};
use core::cmp::Ordering;
use core::fmt;

use super::{
    map_index, HamtCodec, HamtNode, NodePtr, NodeRef, NodeResolver, StructuralHash, HAMT_MAX_DEPTH,
};

pub type Delta<K, V> = Vec<(K, V)>;
pub type DeltaResult<K, V, E> = Result<(Delta<K, V>, Delta<K, V>), E>;

/// Walks both roots and returns the accumulated added/removed leaves.
fn collect_delta<K, V, E>(
    root_a: &NodePtr<K, V>,
    root_b: &NodePtr<K, V>,
    resolver: &mut impl FnMut(&StructuralHash) -> Result<NodePtr<K, V>, E>,
) -> DeltaResult<K, V, HamtTraversalError<E>>
where
    K: Clone + Eq + HamtCodec,
    V: Clone + Eq,
{
    let mut added = Vec::new();
    let mut removed = Vec::new();
    diff_nodes(root_a, root_b, &mut added, &mut removed, resolver, 0)?;
    Ok((added, removed))
}

/// Validates both diff roots before an identity shortcut can hide a misplaced
/// node.
fn validate_roots_at_depth_zero<K, V, E>(
    root_a: &NodePtr<K, V>,
    root_b: &NodePtr<K, V>,
) -> Result<(), HamtTraversalError<E>> {
    check_node_depth(root_a, 0)?;
    check_node_depth(root_b, 0)?;
    Ok(())
}

/// Encodes every leaf key so a collision-bucket merge can compare canonical
/// wire order without re-encoding inside the loop.
fn encoded_keys<K: HamtCodec, V>(leaves: &[(K, V)]) -> Vec<Vec<u8>> {
    leaves
        .iter()
        .map(|(key, _)| {
            let mut bytes = Vec::new();
            key.encode_hamt(&mut bytes);
            bytes
        })
        .collect()
}

/// Computes the delta (added/removed items) between two HAMT root nodes,
/// short-circuiting on identical structural hashes.
///
/// # Errors
/// Returns [`HamtTraversalError::Resolve`] if the `resolver` fails to resolve a lazy node,
/// [`HamtTraversalError::CorruptNode`] if either root has a malformed shape or a misplaced
/// collision bucket, or [`HamtTraversalError::MaxDepthExceeded`] if the diff recurses past the
/// deepest depth a legitimately-built HAMT can have.
pub fn diff_hamt_nodes<K, V, F, E>(
    root_a: &NodePtr<K, V>,
    root_b: &NodePtr<K, V>,
    resolver: &mut F,
) -> DeltaResult<K, V, HamtTraversalError<E>>
where
    K: Clone + Eq + HamtCodec,
    V: Clone + Eq,
    F: NodeResolver<K, V, E>,
{
    // Validate both roots before the shortcut: two structurally identical
    // misplaced collision buckets must still be rejected.
    validate_roots_at_depth_zero(root_a, root_b)?;
    if root_a.structural_hash == root_b.structural_hash {
        return Ok((Vec::new(), Vec::new()));
    }

    collect_delta(root_a, root_b, resolver)
}

/// Rejects a node shape that cannot legally appear at `depth`.
///
/// Collision buckets are terminal: they may only occur exactly at
/// [`HAMT_MAX_DEPTH`]. Everything below that depth must be an ordinary node,
/// and anything beyond it is corruption. Enforcing both directions stops a
/// malformed graph from recursing forever on the collision-bucket shortcut.
pub(super) fn check_node_depth<K, V, E>(
    node: &HamtNode<K, V>,
    depth: usize,
) -> Result<(), HamtTraversalError<E>> {
    if let Some(reason) = node.shape_error() {
        return Err(HamtTraversalError::CorruptNode(reason));
    }
    if depth > HAMT_MAX_DEPTH {
        return Err(HamtTraversalError::MaxDepthExceeded { depth });
    }
    if depth == HAMT_MAX_DEPTH {
        // A legitimately built trie terminates in a collision bucket here; a
        // non-terminal node has run past the usable routing depth.
        if !node.is_collision() {
            return Err(HamtTraversalError::MaxDepthExceeded { depth });
        }
    } else if node.is_collision() {
        return Err(HamtTraversalError::CorruptNode(
            "misplaced collision bucket",
        ));
    }
    Ok(())
}

/// Helper to iterate over the indices of set bits in a 32-bit integer.
fn set_bits(mut bits: u32) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        if bits == 0 {
            None
        } else {
            let bit = bits & bits.wrapping_neg();
            bits &= bits.wrapping_sub(1);
            Some(bit.trailing_zeros() as usize)
        }
    })
}

/// Iterates the union of two nodemaps, yielding each set slot with whether the
/// bit is present in `n_a` and/or `n_b`.
fn union_nodemaps(n_a: u32, n_b: u32) -> impl Iterator<Item = (usize, bool, bool)> {
    set_bits(n_a | n_b).map(move |slot| {
        let bit = 1 << slot;
        (slot, (n_a & bit) != 0, (n_b & bit) != 0)
    })
}

/// Returns `true` when two nodes are known equivalent (identical pointer or
/// structural hash) and the diff can skip them, or an error when `depth` has
/// reached the routing limit.
fn diff_short_circuit<K, V, E>(
    node_a: &NodePtr<K, V>,
    node_b: &NodePtr<K, V>,
    depth: usize,
) -> Result<bool, HamtTraversalError<E>> {
    check_node_depth(node_a, depth)?;
    check_node_depth(node_b, depth)?;
    if Arc::ptr_eq(node_a, node_b) {
        return Ok(true);
    }
    if node_a.structural_hash == node_b.structural_hash {
        return Ok(true);
    }
    Ok(false)
}

/// Recursively compute the structural diff between two HAMT nodes.
/// Appends entries present only in B to `added` and entries present only in A
/// to `removed`; a changed value appears in both with its respective value.
///
/// # Errors
/// Propagates resolver failures as [`HamtTraversalError::Resolve`] and reports
/// [`HamtTraversalError::MaxDepthExceeded`] at the routing limit. Output
/// buffers may contain partial changes on error.
#[allow(clippy::too_many_lines)]
fn diff_nodes<K, V, F, E>(
    node_a: &NodePtr<K, V>,
    node_b: &NodePtr<K, V>,
    added: &mut Vec<(K, V)>,
    removed: &mut Vec<(K, V)>,
    resolver: &mut F,
    depth: usize,
) -> Result<(), HamtTraversalError<E>>
where
    K: Clone + Eq + HamtCodec,
    V: Clone + Eq,
    F: NodeResolver<K, V, E>,
{
    if diff_short_circuit(node_a, node_b, depth)? {
        return Ok(());
    }
    // Both sides are terminal collision buckets with no slot maps, so the
    // bitmap walk below would see them as empty. `diff_short_circuit` has
    // already proven both are collisions exactly at the routing limit, and
    // their leaves are canonically sorted; compare them directly.
    if node_a.is_collision() || node_b.is_collision() {
        let leaves_a = &node_a.leaves;
        let leaves_b = &node_b.leaves;
        // Collision leaves are canonically sorted by encoded key bytes, so a
        // linear merge avoids the quadratic nested scans a bucket-sized pair
        // of long collision lists would otherwise cause.
        let encoded_a = encoded_keys(leaves_a);
        let encoded_b = encoded_keys(leaves_b);
        // `shape_error` checks the collision shape but not public-field key
        // order, so an unverified in-memory bucket could otherwise produce a
        // bogus merge result.
        if !encoded_a.windows(2).all(|window| window[0] < window[1])
            || !encoded_b.windows(2).all(|window| window[0] < window[1])
        {
            return Err(HamtTraversalError::CorruptNode(
                "MTHC keys are not in canonical order",
            ));
        }
        let (mut i, mut j) = (0_usize, 0_usize);
        while i < leaves_a.len() && j < leaves_b.len() {
            let (key_a, value_a) = &leaves_a[i];
            let (key_b, value_b) = &leaves_b[j];
            match encoded_a[i].cmp(&encoded_b[j]) {
                Ordering::Less => {
                    removed.push((key_a.clone(), value_a.clone()));
                    i = i.saturating_add(1);
                }
                Ordering::Greater => {
                    added.push((key_b.clone(), value_b.clone()));
                    j = j.saturating_add(1);
                }
                Ordering::Equal => {
                    if key_a != key_b || value_a != value_b {
                        removed.push((key_a.clone(), value_a.clone()));
                        added.push((key_b.clone(), value_b.clone()));
                    }
                    i = i.saturating_add(1);
                    j = j.saturating_add(1);
                }
            }
        }
        removed.extend(leaves_a[i..].iter().cloned());
        added.extend(leaves_b[j..].iter().cloned());
        return Ok(());
    }
    let next_depth = depth.saturating_add(1);

    // Traverse datamaps. Derive the three disjoint slot classes with bitwise
    // set ops, then walk only the occupied slots instead of all 32.
    let d_a = node_a.datamap;
    let d_b = node_b.datamap;

    let n_a = node_a.nodemap;
    let n_b = node_b.nodemap;
    // A leaf can become a subtree when an insertion shares its slot. Handle
    // those slots together so an unchanged leaf is not reported twice.
    let leaf_a_sub_b = d_a & n_b;
    let sub_a_leaf_b = n_a & d_b;
    let cross = leaf_a_sub_b | sub_a_leaf_b;

    let union = (d_a | d_b) & !cross;
    for slot in set_bits(union) {
        let bit = 1 << slot;
        let in_a = (d_a & bit) != 0;
        let in_b = (d_b & bit) != 0;

        if in_a && in_b {
            let idx_a = map_index(d_a, slot);
            let idx_b = map_index(d_b, slot);
            let (k_a, v_a) = &node_a.leaves[idx_a];
            let (k_b, v_b) = &node_b.leaves[idx_b];
            if k_a != k_b || v_a != v_b {
                removed.push((k_a.clone(), v_a.clone()));
                added.push((k_b.clone(), v_b.clone()));
            }
            continue;
        }
        if in_a {
            let idx_a = map_index(d_a, slot);
            let (k_a, v_a) = &node_a.leaves[idx_a];
            removed.push((k_a.clone(), v_a.clone()));
            continue;
        }
        // `bit` is in `union`, and reaching here means it was neither the
        // `in_a && in_b` nor the `in_a`-only case above, so `in_b` is
        // guaranteed true -- no need to re-test it.
        let idx_b = map_index(d_b, slot);
        let (k_b, v_b) = &node_b.leaves[idx_b];
        added.push((k_b.clone(), v_b.clone()));
    }

    // Traverse nodemaps
    for (slot, in_a, in_b) in union_nodemaps(n_a & !cross, n_b & !cross) {
        if in_a && in_b {
            if let Some(children) = resolve_changed_pair(node_a, node_b, n_a, n_b, slot, resolver)?
            {
                diff_nodes(
                    &children.left,
                    &children.right,
                    added,
                    removed,
                    resolver,
                    next_depth,
                )?;
            }
            continue;
        }
        if in_a {
            let res_a = resolve_only_child(node_a, n_a, slot, resolver)?;
            walk_subtree(&res_a, resolver, next_depth, &mut |n| {
                check_collision_order(n)?;
                removed.extend(n.leaves.iter().cloned());
                Ok(())
            })?;
            continue;
        }
        // `bit` is in `union`, and reaching here means it was neither the
        // `in_a && in_b` nor the `in_a`-only case above, so `in_b` is
        // guaranteed true -- no need to re-test it.
        let res_b = resolve_only_child(node_b, n_b, slot, resolver)?;
        walk_subtree(&res_b, resolver, next_depth, &mut |n| {
            check_collision_order(n)?;
            added.extend(n.leaves.iter().cloned());
            Ok(())
        })?;
    }

    for slot in set_bits(leaf_a_sub_b) {
        diff_leaf_subtree(
            &CrossKindSlot {
                leaf_node: node_a,
                subtree_node: node_b,
                leaf_datamap: d_a,
                subtree_nodemap: n_b,
                slot,
                leaf_is_a: true,
            },
            added,
            removed,
            resolver,
            next_depth,
        )?;
    }

    for slot in set_bits(sub_a_leaf_b) {
        diff_leaf_subtree(
            &CrossKindSlot {
                leaf_node: node_b,
                subtree_node: node_a,
                leaf_datamap: d_b,
                subtree_nodemap: n_a,
                slot,
                leaf_is_a: false,
            },
            added,
            removed,
            resolver,
            next_depth,
        )?;
    }

    Ok(())
}

/// Diffs a leaf on one side against a child subtree on the other side.
/// Entries in the subtree matching the leaf exactly cancel; all other
/// subtree entries belong to the subtree side's delta direction.
struct CrossKindSlot<'a, K, V> {
    leaf_node: &'a NodePtr<K, V>,
    subtree_node: &'a NodePtr<K, V>,
    leaf_datamap: u32,
    subtree_nodemap: u32,
    slot: usize,
    leaf_is_a: bool,
}

/// Appends the changes for a slot holding a leaf on one side and a subtree
/// on the other. `cross.leaf_is_a` selects the leaf's side; exact key/value
/// matches are omitted, while changed values are removed and added.
/// `depth` is the subtree root's depth in the full HAMT.
///
/// # Errors
/// Returns [`HamtTraversalError::Resolve`] on a resolver failure or
/// [`HamtTraversalError::MaxDepthExceeded`] at the routing limit. Output
/// buffers may contain partial changes on error.
fn diff_leaf_subtree<K: Clone + Eq + HamtCodec, V: Clone + Eq, E>(
    cross: &CrossKindSlot<'_, K, V>,
    added: &mut Vec<(K, V)>,
    removed: &mut Vec<(K, V)>,
    resolver: &mut impl FnMut(&StructuralHash) -> Result<NodePtr<K, V>, E>,
    depth: usize,
) -> Result<(), HamtTraversalError<E>> {
    let CrossKindSlot {
        leaf_node,
        subtree_node,
        leaf_datamap,
        subtree_nodemap,
        slot,
        leaf_is_a,
    } = cross;
    let (key, value) = &leaf_node.leaves[map_index(*leaf_datamap, *slot)];
    let child = resolve_only_child(subtree_node, *subtree_nodemap, *slot, resolver)?;
    let subtree_delta = if *leaf_is_a {
        &mut *added
    } else {
        &mut *removed
    };
    let mut unchanged = false;
    walk_subtree(&child, resolver, depth, &mut |node| {
        check_collision_order(node)?;
        for (other_key, other_value) in &node.leaves {
            if other_key == key && other_value == value {
                unchanged = true;
            } else {
                subtree_delta.push((other_key.clone(), other_value.clone()));
            }
        }
        Ok(())
    })?;
    if !unchanged {
        let leaf_delta = if *leaf_is_a {
            &mut *removed
        } else {
            &mut *added
        };
        leaf_delta.push((key.clone(), value.clone()));
    }
    Ok(())
}

/// Resolves a lazy node reference through `resolver`, cloning resolved nodes.
pub(super) fn resolve_node<K, V, F, E>(
    node_ref: &NodeRef<K, V>,
    resolver: &mut F,
) -> Result<NodePtr<K, V>, E>
where
    F: NodeResolver<K, V, E>,
{
    match node_ref {
        NodeRef::Resolved(arc) => Ok(arc.clone()),
        NodeRef::Lazy(hash) => resolver(hash),
    }
}

/// [`resolve_node`] lifted into [`HamtTraversalError`], the error type every
/// node-walking helper in this module (and `mod.rs`'s entry traversal) reports.
pub(super) fn resolve_node_checked<K, V, F, E>(
    node_ref: &NodeRef<K, V>,
    resolver: &mut F,
) -> Result<NodePtr<K, V>, HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    resolve_node(node_ref, resolver).map_err(HamtTraversalError::Resolve)
}

/// The resolved children on the two sides of a diff that need recursive
/// comparison.
struct ChangedChildPair<K, V> {
    left: NodePtr<K, V>,
    right: NodePtr<K, V>,
}

/// For a nodemap slot occupied on both sides of a diff, returns the resolved
/// child pair when their structural hashes differ (so the caller must recurse),
/// or `None` when they are identical (nothing changed below this slot).
fn resolve_changed_pair<K, V, F, E>(
    node_a: &NodePtr<K, V>,
    node_b: &NodePtr<K, V>,
    nodemap_a: u32,
    nodemap_b: u32,
    slot: usize,
    resolver: &mut F,
) -> Result<Option<ChangedChildPair<K, V>>, HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    let child_a = &node_a.children[map_index(nodemap_a, slot)];
    let child_b = &node_b.children[map_index(nodemap_b, slot)];
    if child_a.structural_hash() == child_b.structural_hash() {
        return Ok(None);
    }
    Ok(Some(ChangedChildPair {
        left: resolve_node_checked(child_a, resolver)?,
        right: resolve_node_checked(child_b, resolver)?,
    }))
}

/// Resolves the single child occupying a nodemap `slot` on only one side of a
/// diff.
fn resolve_only_child<K, V, F, E>(
    node: &NodePtr<K, V>,
    nodemap: u32,
    slot: usize,
    resolver: &mut F,
) -> Result<NodePtr<K, V>, HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    resolve_node_checked(&node.children[map_index(nodemap, slot)], resolver)
}

/// Error returned by the node-hash traversal helpers
/// ([`diff_node_hashes`], [`reachable_node_hashes`],
/// [`walk_reachable_node_hashes`]): either the caller-supplied `resolver`
/// failed, or the walk exceeded the crate's internal max-depth bound — the
/// deepest a HAMT this crate builds can ever legitimately be.
///
/// A resolver reading from a store can be handed corrupted or adversarial
/// data — a node whose `nodemap` children chain far deeper than any tree
/// [`build_hamt`](super::build_hamt) could have produced (or, in the limit,
/// a cycle). Recursing on that without a bound risks exhausting the call
/// stack and aborting the whole process; `MaxDepthExceeded` turns that into
/// an ordinary `Result::Err` instead.
#[derive(Debug, PartialEq, Eq)]
pub enum HamtTraversalError<E> {
    /// The resolver failed to load a lazy child the walk needed to descend
    /// into.
    Resolve(E),
    /// A node's shape is illegal for the depth it occupies. This can come
    /// from malformed persisted data, so it is reported rather than treated
    /// as an exhausted depth.
    CorruptNode(&'static str),
    /// The walk recursed past the deepest depth a legitimately-built HAMT
    /// can have, which only happens against corrupted or adversarial node
    /// data.
    MaxDepthExceeded { depth: usize },
}

impl<E> fmt::Display for HamtTraversalError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(err) => write!(f, "hamt traversal resolver failed: {err}"),
            Self::CorruptNode(reason) => write!(f, "hamt traversal corrupt node: {reason}"),
            Self::MaxDepthExceeded { depth } => {
                write!(f, "hamt traversal exceeded max depth at {depth}")
            }
        }
    }
}

impl<E> core::error::Error for HamtTraversalError<E>
where
    E: core::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Resolve(err) => Some(err),
            Self::CorruptNode(_) | Self::MaxDepthExceeded { .. } => None,
        }
    }
}

/// The result of [`diff_node_hashes`]: the node hashes a path-copying
/// mutation superseded vs. the ones it newly created.
///
/// A named struct instead of a `(Vec<_>, Vec<_>)` tuple deliberately, since
/// the two fields hold the same element type in opposite GC roles —
/// swapping them at a call site would type-check silently while inverting
/// refcount increments and decrements. See [`diff_node_hashes`] for the
/// full timing contract these two lists are meant to be used under.
#[derive(Debug, Default)]
pub struct NodeHashDelta {
    /// Node hashes present in `root_a` but not `root_b`. GC candidates once
    /// `root_a` is retired — never delete these while `root_a` is still
    /// live.
    pub superseded_node_hashes: Vec<StructuralHash>,
    /// Node hashes present in `root_b` but not `root_a`. Safe to increment
    /// refcounts for as soon as `root_b` is persisted.
    pub new_node_hashes: Vec<StructuralHash>,
}

/// Returns the internal-node hash delta for one path-copying mutation.
/// `new_node_hashes` are incremented when `root_b` is persisted;
/// `superseded_node_hashes` are decremented only when `root_a` is retired.
/// See the optional `unstable-refcount-gc` integration.
///
/// Pairwise retirement is safe only for a linear root history. If `root_a`
/// has multiple live descendants, use [`walk_reachable_node_hashes`] across
/// every live root: a two-root diff cannot see sharing through a third root.
/// Use [`reachable_node_hashes`] to bootstrap or verify absolute refcounts.
///
/// Adjacent roots take `O(|spine|)`; unrelated roots may take `O(N)`.
///
/// # Errors
/// Returns [`HamtTraversalError::Resolve`] if `resolver` fails, or
/// [`HamtTraversalError::MaxDepthExceeded`] if the walk recurses past the
/// deepest depth a legitimately-built HAMT can have.
pub fn diff_node_hashes<K, V, F, E>(
    root_a: &NodePtr<K, V>,
    root_b: &NodePtr<K, V>,
    resolver: &mut F,
) -> Result<NodeHashDelta, HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    let mut superseded_node_hashes = Vec::new();
    let mut new_node_hashes = Vec::new();
    diff_node_hashes_rec(
        root_a,
        root_b,
        &mut superseded_node_hashes,
        &mut new_node_hashes,
        resolver,
        0,
    )?;
    Ok(NodeHashDelta {
        superseded_node_hashes,
        new_node_hashes,
    })
}

/// Recursively compute the hash differences between two HAMT nodes.
fn diff_node_hashes_rec<K, V, F, E>(
    node_a: &NodePtr<K, V>,
    node_b: &NodePtr<K, V>,
    superseded: &mut Vec<StructuralHash>,
    new: &mut Vec<StructuralHash>,
    resolver: &mut F,
    depth: usize,
) -> Result<(), HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    if diff_short_circuit(node_a, node_b, depth)? {
        return Ok(());
    }

    superseded.push(node_a.structural_hash);
    new.push(node_b.structural_hash);

    let n_a = node_a.nodemap;
    let n_b = node_b.nodemap;

    let next_depth = depth.saturating_add(1);

    for (slot, in_a, in_b) in union_nodemaps(n_a, n_b) {
        if in_a && in_b {
            if let Some(children) = resolve_changed_pair(node_a, node_b, n_a, n_b, slot, resolver)?
            {
                diff_node_hashes_rec(
                    &children.left,
                    &children.right,
                    superseded,
                    new,
                    resolver,
                    next_depth,
                )?;
            }
            continue;
        }
        if in_a {
            let res_a = resolve_only_child(node_a, n_a, slot, resolver)?;
            append_reachable_node_hashes(&res_a, superseded, resolver, next_depth)?;
            continue;
        }
        // `bit` is in `union`, and reaching here means it was neither the
        // `in_a && in_b` nor the `in_a`-only case above, so `in_b` is
        // guaranteed true -- no need to re-test it.
        let res_b = resolve_only_child(node_b, n_b, slot, resolver)?;
        append_reachable_node_hashes(&res_b, new, resolver, next_depth)?;
    }

    Ok(())
}

/// Collects the structural hash of `root` and every internal node reachable
/// from it via `nodemap` children.
///
/// This is the bootstrap/verification primitive for refcount-based garbage
/// collection: a storage backend can sum this over every currently-live root
/// to compute (or double-check) absolute per-hash reference counts, which
/// [`diff_node_hashes`] alone cannot provide since it only reports the delta
/// between two adjacent roots.
///
/// Runs in `O(N)` time where `N` is the number of internal nodes reachable from `root` and
/// may resolve every lazy child along the way.
///
/// # Errors
/// Returns [`HamtTraversalError::Resolve`] if `resolver` fails, or
/// [`HamtTraversalError::MaxDepthExceeded`] if the walk recurses past the
/// deepest depth a legitimately-built HAMT can have.
pub fn reachable_node_hashes<K, V, F, E>(
    root: &NodePtr<K, V>,
    resolver: &mut F,
) -> Result<Vec<StructuralHash>, HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    let mut hashes = Vec::new();
    append_reachable_node_hashes(root, &mut hashes, resolver, 0)?;
    Ok(hashes)
}

/// Walks `root`'s internal-node graph, calling `mark` for every hash.
/// Reuse one caller-owned mark set across roots: a child counted by an earlier
/// call is never resolved, at any depth, so it may already have been reclaimed.
/// Placement is re-checked only within one call: a hash that this call has
/// resolved is resolved and checked again if it appears at a new depth. The
/// root's placement is checked on every call. Multi-root placement
/// revalidation is provided by
/// [`node_reachability_audit`](crate::hamt::node_reachability_audit) and
/// [`bitmap_node_reachability_audit`](crate::hamt::bitmap_node_reachability_audit),
/// which track depths across every root.
/// Use [`reachable_node_hashes`] for a single root.
///
/// # Errors
/// Returns [`HamtTraversalError::Resolve`] if `resolver` fails, or
/// [`HamtTraversalError::MaxDepthExceeded`] if the walk recurses past the
/// deepest depth a legitimately-built HAMT can have.
///
/// Do not reuse the mark set after an error: it may contain an unresolved
/// node whose descendants were never visited.
pub fn walk_reachable_node_hashes<K, V, F, E, M>(
    root: &NodePtr<K, V>,
    resolver: &mut F,
    mark: &mut M,
) -> Result<(), HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
    M: FnMut(StructuralHash) -> bool,
{
    // Validate the root even when its content was already seen by an earlier
    // call; otherwise a shared hash could mask a misplaced bucket at depth 0.
    check_node_depth(root, 0)?;
    let mut local = crate::HashMap::default();
    local.insert(root.structural_hash, 1_u64);
    if !mark(root.structural_hash) {
        return Ok(());
    }
    walk_reachable_children(root, resolver, mark, 0, &mut local, false)
}

/// Reachability walk shared by audits so content marking and placement
/// validation remain distinct across all roots in the audit.
pub(super) fn walk_reachable_node_hashes_with_placements<K, V, F, E, M>(
    root: &NodePtr<K, V>,
    resolver: &mut F,
    mark: &mut M,
    checked_placements: &mut crate::HashMap<StructuralHash, u64>,
) -> Result<(), HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
    M: FnMut(StructuralHash) -> bool,
{
    // Validate the root even when its content was already seen by an earlier
    // call; otherwise a shared hash could mask a misplaced bucket at depth 0.
    check_node_depth(root, 0)?;
    let _ = mark(root.structural_hash);
    let seen = checked_placements
        .get(&root.structural_hash)
        .copied()
        .unwrap_or(0);
    if seen & 1 != 0 {
        return Ok(());
    }
    *checked_placements.entry(root.structural_hash).or_insert(0) |= 1;
    walk_reachable_children(root, resolver, mark, 0, checked_placements, true)
}

/// Resolves a child and validates that its shape is legal at `depth`.
fn resolve_child_at_depth<K, V, F, E>(
    child: &NodeRef<K, V>,
    resolver: &mut F,
    depth: usize,
) -> Result<NodePtr<K, V>, HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    let node = resolve_node_checked(child, resolver)?;
    check_node_depth(&node, depth)?;
    Ok(node)
}

/// Shared reachability walk. `validate_mark_miss` is enabled for multi-root
/// audits, where a marked hash at an unseen depth still has to be resolved to
/// validate that placement. The public single-walk path preserves the GC-safe
/// mark-miss shortcut unless the current walk has already seen that hash.
fn walk_reachable_children<K, V, F, E, M>(
    node: &NodePtr<K, V>,
    resolver: &mut F,
    mark: &mut M,
    depth: usize,
    local: &mut crate::HashMap<StructuralHash, u64>,
    validate_mark_miss: bool,
) -> Result<(), HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
    M: FnMut(StructuralHash) -> bool,
{
    check_node_depth(node, depth)?;
    let next_depth = depth.saturating_add(1);
    let next_bit = 1_u64 << next_depth;
    for child in &node.children {
        let hash = child.structural_hash();
        let newly_marked = mark(hash);
        let depths_seen = local.get(&hash).copied().unwrap_or(0);
        if depths_seen & next_bit != 0 {
            continue;
        }
        let repeated_hash = depths_seen != 0;
        if !newly_marked && !repeated_hash && !validate_mark_miss {
            continue;
        }
        let child_node = resolve_child_at_depth(child, resolver, next_depth)?;
        *local.entry(hash).or_insert(0) |= next_bit;
        walk_reachable_children(
            &child_node,
            resolver,
            mark,
            next_depth,
            local,
            validate_mark_miss,
        )?;
    }
    Ok(())
}

/// Appends the structural hash of `node` and every internal node reachable
/// from it, in pre-order. Shared by [`reachable_node_hashes`] and
/// [`diff_node_hashes`] (to enumerate a whole subtree that only exists on
/// one side of a diff).
fn append_reachable_node_hashes<K, V, F, E>(
    node: &NodePtr<K, V>,
    collection: &mut Vec<StructuralHash>,
    resolver: &mut F,
    depth: usize,
) -> Result<(), HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    walk_subtree(node, resolver, depth, &mut |node| {
        collection.push(node.structural_hash);
        Ok(())
    })
}

/// Rejects a collision bucket whose public-field leaves are not in canonical
/// wire order. Diff walks that emit leaves need this before extending their
/// output; hash-only walks have no key codec and do not call it.
fn check_collision_order<K, V, E>(node: &HamtNode<K, V>) -> Result<(), HamtTraversalError<E>>
where
    K: HamtCodec,
{
    if node.is_collision() {
        super::codec::check_collision_key_order(&node.leaves)
            .map_err(HamtTraversalError::CorruptNode)?;
    }
    Ok(())
}

/// Traverses a subtree in pre-order, resolving every child through the shared
/// resolver and applying the HAMT depth guard at each node. Collection-specific
/// callers provide only the item they want from each node.
fn walk_subtree<K, V, F, E>(
    node: &NodePtr<K, V>,
    resolver: &mut F,
    depth: usize,
    visit: &mut impl FnMut(&NodePtr<K, V>) -> Result<(), HamtTraversalError<E>>,
) -> Result<(), HamtTraversalError<E>>
where
    F: NodeResolver<K, V, E>,
{
    check_node_depth(node, depth)?;
    visit(node)?;
    let next_depth = depth.saturating_add(1);
    for child in &node.children {
        let child_node = resolve_node_checked(child, resolver)?;
        walk_subtree(&child_node, resolver, next_depth, visit)?;
    }
    Ok(())
}
