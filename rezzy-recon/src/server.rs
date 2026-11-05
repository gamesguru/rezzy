// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Responder-side MSC0501 reconciliation digest generation.

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    string::ToString,
    vec::Vec,
};

use crate::EventId;

use super::{
    algebraic::SyndromeSketch,
    triage::{BucketRequest, NodeSummary},
    AccumulatedElement, AlgebraicError, ElementHash, EventIdFormat, FixedWindowElementHash,
    RoomAccumulator, RoomEventIdKind, H64_TRIE_WIDTH,
};
use crate::gf256::Gf256Sketch;

/// Read-only helper over a pre-sorted `h64` index.
///
/// This keeps bucket extraction cheap and explicit without forcing callers into
/// a heavier storage abstraction.
#[derive(Debug, Clone, Copy)]
pub struct H64Index<'a> {
    sorted_h64: &'a [u64],
}

impl<'a> H64Index<'a> {
    /// Creates a view over a pre-sorted `h64` slice.
    #[must_use]
    pub const fn new(sorted_h64: &'a [u64]) -> Self {
        Self { sorted_h64 }
    }

    pub(crate) fn bounds_unchecked(request: &BucketRequest) -> core::ops::Range<u128> {
        let depth = u32::from(request.depth);
        let shift = u32::from(H64_TRIE_WIDTH).saturating_sub(depth);

        // A u128 safely handles (1 << 64) - 1, which cleanly downcasts to u64::MAX
        let prefix_mask = u64::try_from((1_u128 << depth).saturating_sub(1)).unwrap_or(u64::MAX);
        let prefix = request.prefix & prefix_mask;

        let start = u128::from(prefix) << shift;

        // When depth reaches the internal trie width, the shift collapses to 0.
        let end = start.saturating_add(1_u128 << shift);

        start..end
    }

    fn bucket_range_unchecked(&self, request: &BucketRequest) -> core::ops::Range<usize> {
        if request.depth == 0 {
            return 0..self.sorted_h64.len();
        }

        let bounds = Self::bounds_unchecked(request);
        let start_idx = self
            .sorted_h64
            .partition_point(|&x| u128::from(x) < bounds.start);
        let end_idx = start_idx.saturating_add(
            self.sorted_h64[start_idx..].partition_point(|&x| u128::from(x) < bounds.end),
        );

        start_idx..end_idx
    }

    /// Returns the half-open slice range covered by one bucket request.
    ///
    /// # Errors
    /// Returns an error when the request is malformed.
    pub fn bucket_range(
        &self,
        request: &BucketRequest,
    ) -> Result<core::ops::Range<usize>, AlgebraicError> {
        crate::triage::validate_adaptive_bucket_requests(core::slice::from_ref(request))?;
        Ok(self.bucket_range_unchecked(request))
    }

    /// Returns the `h64` slice covered by one bucket request.
    ///
    /// # Errors
    /// Returns an error when the request is malformed.
    pub fn bucket_slice(&self, request: &BucketRequest) -> Result<&'a [u64], AlgebraicError> {
        crate::triage::validate_adaptive_bucket_requests(core::slice::from_ref(request))?;
        let range = self.bucket_range_unchecked(request);
        Ok(&self.sorted_h64[range])
    }

    fn bucket_slice_unchecked(&self, request: &BucketRequest) -> &'a [u64] {
        let range = self.bucket_range_unchecked(request);
        &self.sorted_h64[range]
    }
}

/// Reusable reconciliation view over one room's current frame and `h64` index.
///
/// This is a convenience layer for callers that repeatedly query the same room
/// state: it avoids threading the graph, anchors, and sorted index separately
/// through every call site.
#[derive(Debug, Clone, Copy)]
pub struct ReconciliationContext<
    'a,
    Id: EventId,
    G: ForwardGraph<Id>,
    P: Population = SortedPopulation,
> {
    graph: &'a G,
    frame_anchors: &'a [Id],
    population: &'a P,
}

impl<'a, Id: EventId, G: ForwardGraph<Id>, P: Population> ReconciliationContext<'a, Id, G, P> {
    /// Creates a reusable context for one room/frame snapshot.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::UnsupportedRoomVersion`] for a room whose
    /// event IDs are not reference hashes: an `event_ids` frame MUST NOT be
    /// constructed for it.
    pub const fn new(
        kind: RoomEventIdKind,
        graph: &'a G,
        frame_anchors: &'a [Id],
        population: &'a P,
    ) -> Result<Self, AlgebraicError> {
        if let Err(error) = kind.require_event_ids_frame() {
            return Err(error);
        }
        Ok(Self {
            graph,
            frame_anchors,
            population,
        })
    }

    /// Returns the negotiated frame digest for this room snapshot.
    ///
    /// # Errors
    /// Returns an error if any frame event IDs violate format rules or element
    /// hashing limits.
    pub fn frame_digest(&self) -> Result<RoomAccumulator, AlgebraicError> {
        compute_frame_digest(
            RoomEventIdKind::ReferenceHash,
            self.graph,
            self.frame_anchors,
        )
    }

    /// Like `bucket_sketches`, but also returns each
    /// node's [`NodeSummary`], from the same population object.
    ///
    /// # Errors
    /// Returns an error if any sketches exceed capacity limits or if requests
    /// are invalid.
    pub fn bucket_nodes(
        &self,
        requests: &[BucketRequest],
    ) -> Result<Vec<(SyndromeSketch, NodeSummary)>, AlgebraicError> {
        build_bucket_nodes(self.population, requests)
    }

    /// The population this context serves.
    #[must_use]
    pub const fn population(&self) -> &'a P {
        self.population
    }
}

impl<'a, Id: EventId, G: ForwardGraph<Id>> ReconciliationContext<'a, Id, G, SortedPopulation> {
    /// Returns the `h64` range covered by one bucket request.
    ///
    /// # Errors
    /// Returns an error when the request is malformed.
    pub fn bucket_range(
        &self,
        request: &BucketRequest,
    ) -> Result<core::ops::Range<usize>, AlgebraicError> {
        self.population.index().bucket_range(request)
    }

    /// Returns the `h64` slice covered by one bucket request.
    ///
    /// # Errors
    /// Returns an error when the request is malformed.
    pub fn bucket_slice(&self, request: &BucketRequest) -> Result<&'a [u64], AlgebraicError> {
        self.population.index().bucket_slice(request)
    }

    /// Constructs bucket sketches over the room's sorted `h64` index.
    ///
    /// # Errors
    /// Returns an error if any sketches exceed capacity limits or if requests are invalid.
    pub fn bucket_sketches(
        &self,
        requests: &[BucketRequest],
    ) -> Result<Vec<SyndromeSketch>, AlgebraicError> {
        build_bucket_sketches(self.population.h64s(), requests)
    }
}

/// Abstraction for forward traversal through the room DAG.
///
/// Matrix homeservers typically traverse backwards via `prev_events`.
/// To support MSC0501 causal frame bounding, we must traverse *forwards*
/// to collect all topological descendants of the frame anchors.
pub trait ForwardGraph<Id: EventId> {
    /// Iterator over the children of an event ID.
    type ChildrenIter<'a>: Iterator<Item = &'a Id>
    where
        Self: 'a,
        Id: 'a;

    /// Returns the children of the given event ID.
    /// This represents the forward edges in the causal graph (where a child's
    /// `prev_events` contains `id`).
    fn children<'a>(&'a self, id: &Id) -> Self::ChildrenIter<'a>;

    /// Checks if a given event ID is known to the server.
    /// A known event can be either fully accepted, or rejected (a tombstone).
    /// MSC0501 strictly requires that rejected events are included.
    fn is_known(&self, id: &Id) -> bool;

    /// Returns the string representation of an event ID if available without allocation.
    /// Defaults to `None`, falling back to `Display::to_string`.
    fn event_id_str<'a>(&'a self, _id: &'a Id) -> Option<&'a str> {
        None
    }

    /// Returns the event ID format for a given known event, used to compute
    /// its algebraic digest.
    ///
    /// Implementations MUST return the correct format even for rejected
    /// events (tombstones). If a rejected event's format is misidentified,
    /// its hash will be incorrect and will silently desync the reconciliation set.
    fn event_format(&self, id: &Id) -> EventIdFormat;

    /// Returns the computed `ElementHash` for a given event ID.
    ///
    /// The default implementation resolves `event_id_str` (or `Display`) and `event_format`
    /// to derive `ElementHash::from_matrix_event_id`.
    /// Custom implementations using integer/interned IDs (e.g. `u64` short IDs) can override
    /// this method to compute `ElementHash` directly without string formatting or allocations.
    ///
    /// # Errors
    /// Returns an error if the event ID format is invalid or hashing fails.
    fn event_hash(&self, id: &Id) -> Result<ElementHash, AlgebraicError> {
        let format = self.event_format(id);
        self.event_id_str(id).map_or_else(
            || ElementHash::from_matrix_event_id(&id.to_string(), format),
            |s| ElementHash::from_matrix_event_id(s, format),
        )
    }

    /// Returns the full-digest element for a given event ID, using the same
    /// string and format resolution as [`Self::event_hash`].
    ///
    /// Implementations that override [`Self::event_hash`] to customize hashing
    /// must also override this method if the custom behavior should apply to
    /// full-digest reconciliation. The default derives the canonical digest
    /// from the event ID string; a truncated `ElementHash` cannot recover it.
    ///
    /// # Errors
    /// Returns an error if the event ID format is invalid or hashing fails.
    fn fixed_window_hash(&self, id: &Id) -> Result<FixedWindowElementHash, AlgebraicError> {
        let format = self.event_format(id);
        self.event_id_str(id).map_or_else(
            || FixedWindowElementHash::from_matrix_event_id(&id.to_string(), format),
            |s| FixedWindowElementHash::from_matrix_event_id(s, format),
        )
    }
}

/// Defines a public frame-digest entry point that folds one element type over
/// the shared traversal. Both entry points have identical signatures, so the
/// macro keeps them from drifting apart.
macro_rules! frame_digest_fn {
    ($(#[$doc:meta])* $name:ident, |$graph:ident, $id:ident| $hash_of:expr) => {
        $(#[$doc])*
        pub fn $name<Id: EventId, G: ForwardGraph<Id>>(
            kind: RoomEventIdKind,
            graph: &G,
            frame_anchors: &[Id],
        ) -> Result<RoomAccumulator, AlgebraicError> {
            fold_frame(kind, graph, frame_anchors, |$id| {
                let $graph = graph;
                $hash_of
            })
        }
    };
}

frame_digest_fn! {
    /// Computes the MSC0501 room digest over a negotiated frame.
    ///
    /// The frame is mathematically bounded by the causal graph. The digested
    /// population includes only the known events that **causally succeed** (are
    /// topological descendants of) the anchor antichain. Events that causally
    /// precede the anchor, such as pre-join history, are excluded.
    ///
    /// # Errors
    /// Returns [`AlgebraicError::UnsupportedRoomVersion`] for a room whose event
    /// IDs are not reference hashes (checked once, before any work), or an error
    /// if any frame event IDs violate format rules or element hashing limits.
    compute_frame_digest, |graph, id| graph.event_hash(id)
}

frame_digest_fn! {
    /// [`compute_frame_digest`] over full-digest elements: the same traversal,
    /// accumulating each event's fixed-window residual.
    ///
    /// # Errors
    /// Same as [`compute_frame_digest`]; additionally rejects an all-zero digest.
    compute_fixed_window_frame_digest, |graph, id| {
        let hash = graph.fixed_window_hash(id)?;
        hash.validate()?;
        Ok(hash)
    }
}

fn fold_frame<Id: EventId, G: ForwardGraph<Id>, E: AccumulatedElement>(
    kind: RoomEventIdKind,
    graph: &G,
    frame_anchors: &[Id],
    mut hash_of: impl FnMut(&Id) -> Result<E, AlgebraicError>,
) -> Result<RoomAccumulator, AlgebraicError> {
    kind.require_event_ids_frame()?;
    let mut accumulator = RoomAccumulator::new();
    let mut queue = VecDeque::new();
    let mut visited = BTreeSet::new();

    // Initialize traversal frontier from the frame anchor antichain
    for anchor in frame_anchors {
        if graph.is_known(anchor) {
            queue.push_back(anchor.clone());
            visited.insert(anchor.clone());
        }
    }

    // Breadth-first traversal down the causal graph
    while let Some(current) = queue.pop_front() {
        for child in graph.children(&current) {
            // Unconditionally mark visited to avoid re-traversing unknown forks
            if visited.insert(child.clone()) && graph.is_known(child) {
                accumulator.insert(hash_of(child)?)?;
                queue.push_back(child.clone());
            }
        }
    }

    Ok(accumulator)
}

/// Constructs bucket sketches for the provided MSC0501 triage requests.
///
/// This uses an $O(\log n)$ binary search on a pre-sorted array of 64-bit event IDs,
/// extracting and toggling only the slice of events requested in each bucket.
///
/// # Errors
/// Returns an error if any sketches exceed capacity limits or if requests are invalid.
///
pub fn build_bucket_sketches(
    sorted_h64: &[u64],
    requests: &[BucketRequest],
) -> Result<Vec<SyndromeSketch>, AlgebraicError> {
    crate::triage::validate_adaptive_bucket_requests(requests)?;
    let index = H64Index::new(sorted_h64);

    let mut sketches = Vec::with_capacity(requests.len());

    for request in requests {
        let mut sketch = if request.overflow {
            SyndromeSketch::new_overflow(request.capacity)?
        } else {
            SyndromeSketch::new(request.capacity)?
        };
        // The pre-sorted index makes the bucket's contents a contiguous slice.
        for &h64 in index.bucket_slice_unchecked(request) {
            sketch.toggle(h64)?;
        }

        sketches.push(sketch);
    }

    Ok(sketches)
}

/// In-memory population retaining the full digest required by
/// `algebraic_gf256`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixedWindowPopulation {
    bucket_keys: Vec<u64>,
    elements: Vec<FixedWindowElementHash>,
}

impl FixedWindowPopulation {
    /// Sorts full digests by their fixed big-endian placement key.
    ///
    /// # Errors
    /// Returns `ZeroSketchElement` if a digest is all zero, which is outside
    /// the `PinSketch` element universe for this profile.
    pub fn new(elements: Vec<FixedWindowElementHash>) -> Result<Self, AlgebraicError> {
        if elements
            .iter()
            .any(|element| element.sketch_element() == crate::gf256::Gf256::ZERO)
        {
            return Err(AlgebraicError::ZeroSketchElement);
        }
        let mut elements = elements;
        elements.sort_unstable_by_key(|element| element.bucket_key());
        let bucket_keys = elements
            .iter()
            .map(|element| element.bucket_key())
            .collect();
        Ok(Self {
            bucket_keys,
            elements,
        })
    }

    /// Number of retained full digests.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.elements.len()
    }

    /// Whether this population is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    fn bucket_range(&self, request: &BucketRequest) -> core::ops::Range<usize> {
        H64Index::new(&self.bucket_keys).bucket_range_unchecked(request)
    }

    /// Builds one GF(2^256) sketch for each validated bucket request.
    ///
    /// # Errors
    /// Returns an error for malformed requests, invalid capacities, or a
    /// sketch element outside the nonzero field-element universe.
    pub fn bucket_sketches(
        &self,
        requests: &[BucketRequest],
    ) -> Result<Vec<Gf256Sketch>, AlgebraicError> {
        build_gf256_bucket_sketches(self, requests)
    }

    /// Builds full-field sketches with the existing per-node count/residual.
    ///
    /// # Errors
    /// Returns an error for malformed requests, invalid capacities, or a
    /// sketch element outside the nonzero field-element universe.
    pub fn bucket_nodes(
        &self,
        requests: &[BucketRequest],
    ) -> Result<Vec<(Gf256Sketch, NodeSummary)>, AlgebraicError> {
        build_gf256_bucket_nodes(self, requests)
    }
}

/// Default fixed depth for [`IncrementalFixedWindowPopulation`].
///
/// This is the maximum initial cover depth: one bucket per permitted request in
/// the normal round (`MAX_BUCKETS_PER_ROUND`). Requests shallower than this are
/// answered by `XORing` cached leaf sketches; requests deeper than this are built
/// from the elements inside one leaf.
pub const DEFAULT_BUCKET_CACHE_DEPTH: u8 = crate::client::MAX_BUCKET_ROUND_DEPTH;

/// Maximum sketch capacity maintained per cached leaf.
const CACHED_LEAF_CAPACITY: usize = crate::triage::MAX_BUCKET_SKETCH_CAPACITY;

/// One fixed-depth leaf of an [`IncrementalFixedWindowPopulation`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedBucket {
    sketch: Gf256Sketch,
    count: u64,
    digest: u128,
}

impl CachedBucket {
    fn new() -> Result<Self, AlgebraicError> {
        Ok(Self {
            sketch: Gf256Sketch::new(CACHED_LEAF_CAPACITY)?,
            count: 0,
            digest: 0,
        })
    }
}

/// Incrementally maintained GF(2^256) population with a fixed-depth bucket
/// cache.
///
/// The cache keeps a normal-capacity sketch and [`NodeSummary`] for each prefix
/// at [`DEFAULT_BUCKET_CACHE_DEPTH`] (or a caller-chosen shallower depth).
/// Inserting or removing one element toggles exactly one leaf, so the leaf
/// work is constant in room size; the `BTreeMap` update costs `O(log n)`.
/// Lookups work as follows:
///
/// - requests at or above the cached depth, with capacity at most
///   `MAX_BUCKET_SKETCH_CAPACITY`, XOR the cached leaf sketches and summaries;
/// - everything else (deeper splits, or overflow capacity above the normal
///   maximum) falls back to scanning only the elements inside the requested
///   prefix, not the whole room.
///
/// This is the intended production shape: the level-0/leaf state is sustained by
/// event insertion/removal, while adaptive splitting narrows a failing region
/// before any subset scan is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalFixedWindowPopulation {
    depth: u8,
    leaves: Vec<CachedBucket>,
    /// `(bucket key, full digest) -> multiplicity`. A map rather than a
    /// `Vec` per key avoids one heap allocation for every distinct 64-bit
    /// prefix in a large room.
    elements: BTreeMap<(u64, [u8; 32]), u64>,
    len: usize,
}

impl IncrementalFixedWindowPopulation {
    /// Builds a cache from an existing population at `depth`.
    ///
    /// A depth of 7 covers the normal 128-bucket first round. Larger depths
    /// increase memory exponentially and are rejected above
    /// [`crate::client::MAX_BUCKET_ROUND_DEPTH`].
    ///
    /// # Errors
    /// Returns [`AlgebraicError::InvalidBucketIndex`] for a depth above the
    /// normal round cap, or [`AlgebraicError::ZeroSketchElement`] for an
    /// all-zero digest.
    pub fn new(
        elements: impl IntoIterator<Item = FixedWindowElementHash>,
        depth: u8,
    ) -> Result<Self, AlgebraicError> {
        if depth > crate::client::MAX_BUCKET_ROUND_DEPTH {
            return Err(AlgebraicError::InvalidBucketIndex);
        }
        let leaf_count = 1_usize
            .checked_shl(u32::from(depth))
            .ok_or(AlgebraicError::InvalidBucketIndex)?;
        let leaves = (0..leaf_count)
            .map(|_| CachedBucket::new())
            .collect::<Result<Vec<_>, _>>()?;
        let mut population = Self {
            depth,
            leaves,
            elements: BTreeMap::new(),
            len: 0,
        };
        for element in elements {
            population.insert(element)?;
        }
        Ok(population)
    }

    /// Builds a cache at [`DEFAULT_BUCKET_CACHE_DEPTH`].
    ///
    /// # Errors
    /// Same as [`Self::new`].
    pub fn with_default_depth(
        elements: impl IntoIterator<Item = FixedWindowElementHash>,
    ) -> Result<Self, AlgebraicError> {
        Self::new(elements, DEFAULT_BUCKET_CACHE_DEPTH)
    }

    /// Number of retained elements.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the population is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Cached prefix depth.
    #[must_use]
    pub const fn depth(&self) -> u8 {
        self.depth
    }

    /// Adds one element, updating its cached leaf in O(sketch capacity).
    ///
    /// # Errors
    /// Returns [`AlgebraicError::ZeroSketchElement`] for the reserved all-zero
    /// digest, or [`AlgebraicError::CountOverflow`] at `u64::MAX` elements.
    pub fn insert(&mut self, element: FixedWindowElementHash) -> Result<(), AlgebraicError> {
        element.validate()?;
        let key = element.bucket_key();
        let index = self.leaf_index(key);
        let leaf = &mut self.leaves[index];
        leaf.sketch.toggle(element.sketch_element())?;
        leaf.count = leaf
            .count
            .checked_add(1)
            .ok_or(AlgebraicError::CountOverflow)?;
        leaf.digest ^= element.residual();
        let multiplicity = self.elements.entry((key, element.digest())).or_insert(0);
        *multiplicity = multiplicity
            .checked_add(1)
            .ok_or(AlgebraicError::CountOverflow)?;
        self.len = self
            .len
            .checked_add(1)
            .ok_or(AlgebraicError::CountOverflow)?;
        Ok(())
    }

    /// Removes one matching element, updating its cached leaf.
    ///
    /// Returns `false` when the element is not present. Insertion and removal
    /// are the same XOR toggle, so a removal does not need to know whether the
    /// element was local or remote.
    pub fn remove(&mut self, element: FixedWindowElementHash) -> bool {
        let key = element.bucket_key();
        let element_key = (key, element.digest());
        let Some(multiplicity) = self.elements.get_mut(&element_key) else {
            return false;
        };
        if *multiplicity == 1 {
            self.elements.remove(&element_key);
        } else {
            *multiplicity = multiplicity.saturating_sub(1);
        }

        let index = self.leaf_index(key);
        let leaf = &mut self.leaves[index];
        // The element was validated on insert, so this cannot fail; keeping the
        // result non-panicking makes `remove` safe for callers that race a
        // concurrent mutation.
        let _ = leaf.sketch.toggle(element.sketch_element());
        leaf.count = leaf.count.saturating_sub(1);
        leaf.digest ^= element.residual();
        self.len = self.len.saturating_sub(1);
        true
    }

    /// Builds one sketch per validated bucket request.
    ///
    /// # Errors
    /// Returns an error for malformed requests, invalid capacities, or a
    /// sketch element outside the nonzero field-element universe.
    pub fn bucket_sketches(
        &self,
        requests: &[BucketRequest],
    ) -> Result<Vec<Gf256Sketch>, AlgebraicError> {
        crate::triage::validate_adaptive_bucket_requests(requests)?;
        requests
            .iter()
            .map(|request| self.node_for(request).map(|(sketch, _)| sketch))
            .collect()
    }

    /// Builds sketches and summaries for validated bucket requests.
    ///
    /// # Errors
    /// Same as [`Self::bucket_sketches`].
    pub fn bucket_nodes(
        &self,
        requests: &[BucketRequest],
    ) -> Result<Vec<(Gf256Sketch, NodeSummary)>, AlgebraicError> {
        crate::triage::validate_adaptive_bucket_requests(requests)?;
        requests
            .iter()
            .map(|request| self.node_for(request))
            .collect()
    }

    fn leaf_index(&self, key: u64) -> usize {
        if self.depth == 0 {
            0
        } else {
            let shift = 64_u32.saturating_sub(u32::from(self.depth));
            usize::try_from(key >> shift).unwrap_or(0)
        }
    }

    fn leaf_range(&self, request: &BucketRequest) -> (usize, usize) {
        let shift = self.depth.saturating_sub(request.depth);
        let first = request.prefix << shift;
        let last = request.prefix.saturating_add(1) << shift;
        (
            usize::try_from(first).unwrap_or(0),
            usize::try_from(last).unwrap_or(self.leaves.len()),
        )
    }

    fn node_for(
        &self,
        request: &BucketRequest,
    ) -> Result<(Gf256Sketch, NodeSummary), AlgebraicError> {
        if request.depth <= self.depth && request.capacity <= CACHED_LEAF_CAPACITY {
            let (first, last) = self.leaf_range(request);
            let mut sketch = Gf256Sketch::new(request.capacity)?;
            let mut summary = NodeSummary::default();
            for leaf in &self.leaves[first..last] {
                sketch.xor_prefix(&leaf.sketch, request.capacity)?;
                summary.count = summary
                    .count
                    .checked_add(leaf.count)
                    .ok_or(AlgebraicError::CountOverflow)?;
                summary.digest ^= leaf.digest;
            }
            Ok((sketch, summary))
        } else {
            self.scan_node(request)
        }
    }

    fn scan_node(
        &self,
        request: &BucketRequest,
    ) -> Result<(Gf256Sketch, NodeSummary), AlgebraicError> {
        let mut sketch = Gf256Sketch::new(request.capacity)?;
        let mut summary = NodeSummary::default();
        if request.depth == 0 {
            for (&(_, digest), &multiplicity) in &self.elements {
                Self::add_repeated(
                    &mut sketch,
                    &mut summary,
                    FixedWindowElementHash::from_digest32(digest),
                    multiplicity,
                )?;
            }
        } else {
            let shift = 64_u32.saturating_sub(u32::from(request.depth));
            let start = request.prefix << shift;
            let start_key = (start, [0; 32]);
            let width = 1_u64 << shift;
            if let Some(end) = start.checked_add(width) {
                for (&(_, digest), &multiplicity) in self.elements.range(start_key..(end, [0; 32]))
                {
                    Self::add_repeated(
                        &mut sketch,
                        &mut summary,
                        FixedWindowElementHash::from_digest32(digest),
                        multiplicity,
                    )?;
                }
            } else {
                // The last depth-32 prefix ends at 2^64, which is not a valid
                // `u64` range bound. An unbounded tail includes it exactly.
                for (&(_, digest), &multiplicity) in self.elements.range(start_key..) {
                    Self::add_repeated(
                        &mut sketch,
                        &mut summary,
                        FixedWindowElementHash::from_digest32(digest),
                        multiplicity,
                    )?;
                }
            }
        }
        Ok((sketch, summary))
    }

    fn add_repeated(
        sketch: &mut Gf256Sketch,
        summary: &mut NodeSummary,
        element: FixedWindowElementHash,
        multiplicity: u64,
    ) -> Result<(), AlgebraicError> {
        // `multiplicity` copies of the same element toggle the sketch (and
        // the residual digest) only when the count is odd; the count always
        // adds the full multiplicity.
        if multiplicity % 2 == 1 {
            sketch.toggle(element.sketch_element())?;
            summary.digest ^= element.residual();
        }
        summary.count = summary
            .count
            .checked_add(multiplicity)
            .ok_or(AlgebraicError::CountOverflow)?;
        Ok(())
    }
}

/// Builds GF(2^256) full-digest sketches for localized bucket requests.
///
/// The population must be indexed by `FixedWindowPopulation`; this cannot be
/// derived from `SortedPopulation`, which has already discarded digest bytes.
///
/// # Errors
/// Returns an error when requests are invalid or a capacity is unsupported.
pub fn build_gf256_bucket_sketches(
    population: &FixedWindowPopulation,
    requests: &[BucketRequest],
) -> Result<Vec<Gf256Sketch>, AlgebraicError> {
    crate::triage::validate_adaptive_bucket_requests(requests)?;
    let mut sketches = Vec::with_capacity(requests.len());
    for request in requests {
        let mut sketch = Gf256Sketch::new(request.capacity)?;
        for element in &population.elements[population.bucket_range(request)] {
            sketch.toggle(element.sketch_element())?;
        }
        sketches.push(sketch);
    }
    Ok(sketches)
}

/// Builds GF(2^256) full-digest sketches and matching existing node summaries.
///
/// # Errors
/// Returns an error when requests are invalid or a capacity is unsupported.
pub fn build_gf256_bucket_nodes(
    population: &FixedWindowPopulation,
    requests: &[BucketRequest],
) -> Result<Vec<(Gf256Sketch, NodeSummary)>, AlgebraicError> {
    crate::triage::validate_adaptive_bucket_requests(requests)?;
    let mut nodes = Vec::with_capacity(requests.len());
    for request in requests {
        let range = population.bucket_range(request);
        let mut sketch = Gf256Sketch::new(request.capacity)?;
        let mut summary_count = 0_u64;
        let mut summary_digest = 0_u128;
        for element in &population.elements[range] {
            sketch.toggle(element.sketch_element())?;
            summary_count = summary_count
                .checked_add(1)
                .ok_or(AlgebraicError::CountOverflow)?;
            summary_digest ^= element.residual();
        }
        nodes.push((
            sketch,
            NodeSummary {
                count: summary_count,
                digest: summary_digest,
            },
        ));
    }
    Ok(nodes)
}

/// A population sorted by `(h64, h128)`, with the two columns kept aligned.
///
/// Building it from [`ElementHash`]es sorts once, so a node's elements are a
/// contiguous slice of both columns and the `h64 -> h128` map is multi-valued
/// by construction: [`candidates`](Self::candidates) returns every element
/// sharing an `h64`. There is no way to construct it with misaligned columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SortedPopulation {
    h64: Vec<u64>,
    h128: Vec<u128>,
}

impl SortedPopulation {
    /// Sorts `elements` by `(h64, h128)`.
    #[must_use]
    pub fn new(mut elements: Vec<ElementHash>) -> Self {
        elements.sort_unstable_by_key(|e| (e.h64, e.h128));
        Self {
            h64: elements.iter().map(|e| e.h64).collect(),
            h128: elements.iter().map(|e| e.h128).collect(),
        }
    }

    /// Number of elements.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.h64.len()
    }

    /// Whether the population is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.h64.is_empty()
    }

    /// The sorted `h64` column.
    #[must_use]
    pub fn h64s(&self) -> &[u64] {
        &self.h64
    }

    /// The `h128` column, aligned with [`h64s`](Self::h64s).
    #[must_use]
    pub fn h128s(&self) -> &[u128] {
        &self.h128
    }

    /// An [`H64Index`] over this population's `h64` column.
    #[must_use]
    pub fn index(&self) -> H64Index<'_> {
        H64Index::new(&self.h64)
    }

    fn node_range(&self, node: (u8, u64)) -> core::ops::Range<usize> {
        self.index()
            .bucket_range_unchecked(&BucketRequest::new(node.0, node.1, 1))
    }

    /// The `h128` of every element whose `h64` is `root`, in order; empty when
    /// the root is absent.
    #[must_use]
    pub fn candidates(&self, root: u64) -> &[u128] {
        let start = self.h64.partition_point(|&x| x < root);
        let end = self.h64.partition_point(|&x| x <= root);
        &self.h128[start..end]
    }
}

/// A room's population as the reconciliation exchange reads it: per-node
/// summaries, the `h64`s of a node, and the `h128`s behind a decoded root.
///
/// An in-memory [`SortedPopulation`] implements it, and a persisted store can
/// serve the same reads from a pinned snapshot. Nodes are `(depth, prefix)`;
/// callers validate them (see `validate_bucket_requests`) before asking.
pub trait Population {
    /// The count and `h128` XOR of the elements inside `node`.
    ///
    /// # Errors
    /// Returns an error if the count overflows.
    fn node_summary(&self, node: (u8, u64)) -> Result<NodeSummary, AlgebraicError>;

    /// Calls `f` with the `h64` of every element inside `node`, in no
    /// particular order: toggling a sketch is an XOR, so order never matters,
    /// and a persisted store need not merge its runs to promise one.
    fn for_each_h64_in(&self, node: (u8, u64), f: &mut dyn FnMut(u64));

    /// Appends to `out` the `h128` of every element whose `h64` is `root`, in
    /// no particular order (the verifier XORs them).
    fn candidates_into(&self, root: u64, out: &mut Vec<u128>);
}

impl Population for SortedPopulation {
    fn node_summary(&self, node: (u8, u64)) -> Result<NodeSummary, AlgebraicError> {
        let range = self.node_range(node);
        NodeSummary::from_h128s(&self.h128[range])
    }

    fn for_each_h64_in(&self, node: (u8, u64), f: &mut dyn FnMut(u64)) {
        for &h64 in &self.h64[self.node_range(node)] {
            f(h64);
        }
    }

    fn candidates_into(&self, root: u64, out: &mut Vec<u128>) {
        out.extend_from_slice(self.candidates(root));
    }
}

/// Like [`build_bucket_sketches`], but also returns each node's
/// [`NodeSummary`] (count and `h128` XOR), computed over the same slice that
/// is toggled into the sketch.
///
/// # Errors
/// Returns an error if a sketch exceeds capacity limits or the requests are
/// invalid.
pub fn build_bucket_nodes<P: Population + ?Sized>(
    population: &P,
    requests: &[BucketRequest],
) -> Result<Vec<(SyndromeSketch, NodeSummary)>, AlgebraicError> {
    crate::triage::validate_adaptive_bucket_requests(requests)?;
    let mut nodes = Vec::with_capacity(requests.len());
    for request in requests {
        let node = (request.depth, request.prefix);
        let mut sketch = if request.overflow {
            SyndromeSketch::new_overflow(request.capacity)?
        } else {
            SyndromeSketch::new(request.capacity)?
        };
        let mut toggled = Ok(());
        population.for_each_h64_in(node, &mut |h64| {
            if toggled.is_ok() {
                toggled = sketch.toggle(h64);
            }
        });
        toggled?;
        nodes.push((sketch, population.node_summary(node)?));
    }
    Ok(nodes)
}

// =========================================================================
// Sketch Builder and Budget Planner Logic
// =========================================================================

/// Work limits applied while building sketches for a request.
pub struct SketchPolicy {
    /// Maximum total elements to process across all returned sketches.
    pub max_aggregate_work: usize,
    /// The absolute ceiling where a single slice proves the difference is pathological.
    pub hard_fallback_threshold: usize,
}

/// Outcome of building sketches for a set of bucket requests.
pub enum SketchResult {
    /// A sketch for every request, in order.
    Success(Vec<(SyndromeSketch, BucketRequest)>),
    /// The difference is too large to sketch; fall back to range sync.
    FallbackToRangeSync,
}

/// A responder-side materializer that validates requested slices before paying the O(S) cost to build sketches.
pub struct SketchBuilder<'a> {
    index: &'a H64Index<'a>,
    policy: SketchPolicy,
}

impl<'a> SketchBuilder<'a> {
    #[must_use]
    /// Builds a sketch builder over `index` under `policy`.
    pub const fn new(index: &'a H64Index<'a>, policy: SketchPolicy) -> Self {
        Self { index, policy }
    }

    /// Processes incoming requests, rejecting oversized slices rather than
    /// splitting them server-side, while enforcing total work budgets.
    ///
    /// # Errors
    /// Returns an error if sketch creation fails algebraically.
    pub fn build(
        &self,
        initial_requests: &[BucketRequest],
    ) -> Result<SketchResult, AlgebraicError> {
        let mut total_work: usize = 0;

        crate::triage::validate_adaptive_bucket_requests(initial_requests)?;
        for req in initial_requests {
            let range = self.index.bucket_range_unchecked(req);
            let slice_len = range.len();

            if slice_len > self.policy.hard_fallback_threshold {
                return Ok(SketchResult::FallbackToRangeSync);
            }

            total_work = total_work.saturating_add(slice_len);
            if total_work > self.policy.max_aggregate_work {
                return Ok(SketchResult::FallbackToRangeSync);
            }
        }

        let mut sketches = Vec::with_capacity(initial_requests.len());
        for req in initial_requests {
            let mut sketch = if req.overflow {
                SyndromeSketch::new_overflow(req.capacity)?
            } else {
                SyndromeSketch::new(req.capacity)?
            };
            for &h64 in self.index.bucket_slice_unchecked(req) {
                sketch.toggle(h64)?;
            }
            sketches.push((sketch, *req));
        }

        Ok(SketchResult::Success(sketches))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::fmt;

    #[test]
    fn fixed_window_bucket_builder_uses_big_endian_prefixes_and_full_elements() {
        let mut in_bucket = [0_u8; 32];
        in_bucket[..8].copy_from_slice(&0x1200_0000_0000_1234_u64.to_be_bytes());
        in_bucket[8..16].copy_from_slice(&7_u64.to_be_bytes());
        in_bucket[31] = 1;
        let mut other_bucket = [0_u8; 32];
        other_bucket[..8].copy_from_slice(&0x9200_0000_0000_1234_u64.to_be_bytes());
        other_bucket[31] = 2;
        let population = FixedWindowPopulation::new(vec![
            FixedWindowElementHash::from_digest32(other_bucket),
            FixedWindowElementHash::from_digest32(in_bucket),
        ])
        .unwrap();
        let request = BucketRequest::new(8, 0x12, 4);
        let built = population.bucket_nodes(&[request]).unwrap();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].0.capacity(), 4);
        assert_eq!(built[0].1.count, 1);
        assert_eq!(built[0].1.digest, 1);
        assert_eq!(
            built[0].0.coordinates()[0],
            FixedWindowElementHash::from_digest32(in_bucket).sketch_element()
        );
    }

    #[test]
    fn fixed_window_population_rejects_zero_digest() {
        assert_eq!(
            FixedWindowPopulation::new(vec![FixedWindowElementHash::from_digest32([0; 32])]),
            Err(AlgebraicError::ZeroSketchElement)
        );
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct MockId(String);

    impl fmt::Display for MockId {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    struct MockGraph {
        forward_edges: BTreeMap<MockId, Vec<MockId>>,
        known_events: BTreeSet<MockId>,
    }

    impl MockGraph {
        const fn new() -> Self {
            Self {
                forward_edges: BTreeMap::new(),
                known_events: BTreeSet::new(),
            }
        }

        fn add_edge(&mut self, parent: &str, child: &str) {
            self.forward_edges
                .entry(MockId(parent.to_string()))
                .or_default()
                .push(MockId(child.to_string()));
            self.known_events.insert(MockId(parent.to_string()));
            self.known_events.insert(MockId(child.to_string()));
        }
    }

    impl ForwardGraph<MockId> for MockGraph {
        type ChildrenIter<'a> = core::slice::Iter<'a, MockId>;

        fn children<'a>(&'a self, id: &MockId) -> Self::ChildrenIter<'a> {
            self.forward_edges
                .get(id)
                .map_or_else(|| [].iter(), |children| children.iter())
        }

        fn is_known(&self, id: &MockId) -> bool {
            self.known_events.contains(id)
        }

        fn event_format(&self, _id: &MockId) -> EventIdFormat {
            EventIdFormat::V4Plus
        }

        fn event_hash(&self, id: &MockId) -> Result<ElementHash, AlgebraicError> {
            Ok(ElementHash::from_opaque_bytes(id.0.as_bytes()))
        }

        fn fixed_window_hash(&self, id: &MockId) -> Result<FixedWindowElementHash, AlgebraicError> {
            Ok(mock_fixed_hash(&id.0))
        }
    }

    fn mock_fixed_hash(id: &str) -> FixedWindowElementHash {
        let seed = id
            .bytes()
            .fold(7_u64, |acc, b| acc.wrapping_mul(131) ^ u64::from(b));
        FixedWindowElementHash::from_digest32(crate::test_util::digest32(seed))
    }

    fn id(s: &str) -> MockId {
        MockId(s.to_string())
    }

    fn hash1() -> ElementHash {
        ElementHash::from_opaque_bytes(b"$1")
    }

    fn hash2() -> ElementHash {
        ElementHash::from_opaque_bytes(b"$2")
    }

    const fn four_entry_index() -> [u64; 4] {
        [
            0x0000_0001_0000_0001,
            0x0000_0001_0000_0002,
            0x0000_0002_0000_0001,
            0x0000_0003_0000_0001,
        ]
    }

    const fn sketch_builder<'a>(
        index: &'a H64Index<'a>,
        max_aggregate_work: usize,
        hard_fallback_threshold: usize,
    ) -> SketchBuilder<'a> {
        SketchBuilder::new(
            index,
            SketchPolicy {
                max_aggregate_work,
                hard_fallback_threshold,
            },
        )
    }

    fn single_bucket_roots(
        sorted_h64: &[u64],
        request: BucketRequest,
        capacity: usize,
    ) -> Vec<u64> {
        let requests = [request];
        let sketches = build_bucket_sketches(sorted_h64, &requests).unwrap();
        assert_eq!(sketches.len(), 1);
        sketches[0].clone().decode_elements(capacity).unwrap()
    }

    fn assert_fallback(builder: &SketchBuilder<'_>, requests: &[BucketRequest]) {
        let result = builder.build(requests).unwrap();
        assert!(matches!(result, SketchResult::FallbackToRangeSync));
    }

    #[test]
    fn test_event_id_str_some_branch() {
        // Exercises the `Some(...)` arm of the default `event_hash` impl directly,
        // without needing a full graph traversal.
        struct StringIdGraph;
        impl ForwardGraph<MockId> for StringIdGraph {
            type ChildrenIter<'a> = core::slice::Iter<'a, MockId>;

            fn children<'a>(&'a self, _id: &MockId) -> Self::ChildrenIter<'a> {
                [].iter()
            }

            fn is_known(&self, _id: &MockId) -> bool {
                true
            }

            fn event_id_str<'a>(&'a self, id: &'a MockId) -> Option<&'a str> {
                Some(&id.0)
            }

            fn event_format(&self, _id: &MockId) -> EventIdFormat {
                EventIdFormat::V4Plus
            }
        }

        let valid_v4_id = "$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let event_id = id(valid_v4_id);
        assert!(StringIdGraph.is_known(&event_id));
        assert_eq!(StringIdGraph.children(&event_id).next(), None);

        let hash = StringIdGraph.event_hash(&event_id).unwrap();
        assert_eq!(
            hash,
            ElementHash::from_matrix_event_id(valid_v4_id, EventIdFormat::V4Plus).unwrap()
        );
    }

    #[test]
    fn test_custom_event_hash_override() {
        struct CustomHashGraph(MockGraph);
        impl ForwardGraph<MockId> for CustomHashGraph {
            type ChildrenIter<'a> = core::slice::Iter<'a, MockId>;

            fn children<'a>(&'a self, id: &MockId) -> Self::ChildrenIter<'a> {
                self.0.children(id)
            }

            fn is_known(&self, id: &MockId) -> bool {
                self.0.is_known(id)
            }

            fn event_format(&self, id: &MockId) -> EventIdFormat {
                self.0.event_format(id)
            }

            fn event_hash(&self, id: &MockId) -> Result<ElementHash, AlgebraicError> {
                // Deliberately distinct from the default legacy hash of `id.0`
                // (which would hash "$child") so the assertion below can only
                // pass if traversal actually dispatches through this override.
                Ok(ElementHash::from_opaque_bytes(
                    alloc::format!("$custom-{}", id.0).as_bytes(),
                ))
            }
        }

        let mut base = MockGraph::new();
        base.add_edge("$anchor", "$child");
        let custom_graph = CustomHashGraph(base);

        assert_eq!(
            custom_graph.event_format(&id("$anchor")),
            EventIdFormat::V4Plus
        );

        let digest = compute_frame_digest(
            RoomEventIdKind::ReferenceHash,
            &custom_graph,
            &[id("$anchor")],
        )
        .unwrap();

        let mut expected = RoomAccumulator::new();
        expected
            .insert(ElementHash::from_opaque_bytes(b"$custom-$child"))
            .unwrap();

        assert_eq!(digest.digest(), expected.digest());
        assert_eq!(digest.known_event_count(), 1);
    }

    #[test]
    fn fixed_window_frame_digest_uses_full_digest_residuals() {
        let mut graph = MockGraph::new();
        graph.add_edge("$genesis", "$anchor");
        graph.add_edge("$anchor", "$child1");
        graph.add_edge("$anchor", "$child2");
        graph.add_edge("$child1", "$grandchild");
        graph.add_edge("$child2", "$grandchild");

        let digest = compute_fixed_window_frame_digest(
            RoomEventIdKind::ReferenceHash,
            &graph,
            &[id("$anchor")],
        )
        .unwrap();

        let mut expected = RoomAccumulator::new();
        for name in ["$child1", "$child2", "$grandchild"] {
            expected.insert(mock_fixed_hash(name)).unwrap();
        }
        assert_eq!(digest, expected);
        assert_eq!(digest.known_event_count(), 3);
        // The legacy fold over the same frame is a different digest.
        let legacy =
            compute_frame_digest(RoomEventIdKind::ReferenceHash, &graph, &[id("$anchor")]).unwrap();
        assert_ne!(legacy.digest(), digest.digest());
    }

    #[test]
    fn tests_frame_bounds() {
        let mut graph = MockGraph::new();
        // pre-join history
        graph.add_edge("$genesis", "$prejoin1");
        graph.add_edge("$prejoin1", "$anchor");
        // in frame
        graph.add_edge("$anchor", "$child1");
        graph.add_edge("$anchor", "$child2");
        graph.add_edge("$child1", "$grandchild");
        graph.add_edge("$child2", "$grandchild");

        let digest =
            compute_frame_digest(RoomEventIdKind::ReferenceHash, &graph, &[id("$anchor")]).unwrap();

        let mut expected = RoomAccumulator::new();
        expected
            .insert(ElementHash::from_opaque_bytes(b"$child1"))
            .unwrap();
        expected
            .insert(ElementHash::from_opaque_bytes(b"$child2"))
            .unwrap();
        expected
            .insert(ElementHash::from_opaque_bytes(b"$grandchild"))
            .unwrap();

        assert_eq!(digest.digest(), expected.digest());
        assert_eq!(digest.known_event_count(), 3);
    }

    #[test]
    fn tests_outlier_quarantine() {
        let mut graph = MockGraph::new();
        graph.add_edge("$anchor", "$child");

        // Disconnected outlier
        graph.known_events.insert(id("$outlier"));

        let digest =
            compute_frame_digest(RoomEventIdKind::ReferenceHash, &graph, &[id("$anchor")]).unwrap();

        let mut expected = RoomAccumulator::new();
        expected
            .insert(ElementHash::from_opaque_bytes(b"$child"))
            .unwrap();

        assert_eq!(digest.digest(), expected.digest());
    }

    #[test]
    fn test_build_bucket_sketches_exact_match() {
        use crate::triage::BucketRequest;
        let h1 = hash1();

        let bucket_idx = h1.h64 >> 56;
        let roots = single_bucket_roots(&[h1.h64], BucketRequest::new(8, bucket_idx, 4), 4);

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], h1.h64);
    }

    #[test]
    fn test_h64_index_bucket_slice() {
        use crate::triage::BucketRequest;

        let sorted_h64 = four_entry_index();
        let index = H64Index::new(&sorted_h64);
        let request = BucketRequest::new(32, 1, 4);

        let slice = index.bucket_slice(&request).unwrap();
        assert_eq!(slice, &[0x0000_0001_0000_0001, 0x0000_0001_0000_0002]);
    }

    /// Two events sharing an `h64` (a 64-bit collision) occupy adjacent slots
    /// in the sorted index. `bucket_slice` returns both -- the index is a
    /// multiset -- but toggling an element twice is the identity in
    /// characteristic two, so the pair contributes nothing to the sketch and
    /// can never be decoded as a root. Only the 128-bit residual can notice.
    #[test]
    fn duplicate_h64_is_returned_twice_but_cancels_in_the_sketch() {
        use crate::triage::BucketRequest;

        let shared = 0x0000_0001_0000_0002;
        let sorted_h64 = [0x0000_0001_0000_0001, shared, shared, 0x0000_0002_0000_0001];
        let request = BucketRequest::new(32, 1, 4);

        let slice = H64Index::new(&sorted_h64).bucket_slice(&request).unwrap();
        assert_eq!(slice, &[0x0000_0001_0000_0001, shared, shared]);

        let sketch = &build_bucket_sketches(&sorted_h64, &[request]).unwrap()[0];
        let mut singleton = SyndromeSketch::new(4).unwrap();
        singleton.toggle(0x0000_0001_0000_0001).unwrap();
        assert_eq!(sketch, &singleton, "the colliding pair must cancel out");
        assert_eq!(
            sketch.decode_elements(4).unwrap(),
            vec![0x0000_0001_0000_0001]
        );
    }

    #[test]
    fn duplicate_h64_alone_leaves_an_empty_sketch() {
        use crate::triage::BucketRequest;

        let sorted_h64 = [7, 7];
        let sketch =
            &build_bucket_sketches(&sorted_h64, &[BucketRequest::new(0, 0, 2)]).unwrap()[0];
        assert_eq!(sketch, &SyndromeSketch::new(2).unwrap());
        assert_eq!(sketch.decode_elements(2).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn test_h64_index_bucket_range() {
        use crate::triage::BucketRequest;

        let sorted_h64 = four_entry_index();
        let index = H64Index::new(&sorted_h64);
        let request = BucketRequest::new(32, 1, 4);

        let range = index.bucket_range(&request).unwrap();
        assert_eq!(range, 0..2);
    }

    #[test]
    fn test_reconciliation_context_delegates_room_lookups() {
        use crate::triage::BucketRequest;

        let mut graph = MockGraph::new();
        graph.add_edge("$anchor", "$child");
        graph.add_edge("$child", "$grandchild");

        let child = ElementHash::from_opaque_bytes(b"$child");
        let grandchild = ElementHash::from_opaque_bytes(b"$grandchild");
        let population = SortedPopulation::new(vec![grandchild, child]);
        let sorted_h64 = population.h64s().to_vec();
        let anchors = [id("$anchor")];

        let context = ReconciliationContext::new(
            RoomEventIdKind::ReferenceHash,
            &graph,
            &anchors,
            &population,
        )
        .unwrap();
        let digest = context.frame_digest().unwrap();

        let mut expected = RoomAccumulator::new();
        expected.insert(child).unwrap();
        expected.insert(grandchild).unwrap();
        assert_eq!(digest.digest(), expected.digest());
        assert_eq!(digest.known_event_count(), 2);

        let request = BucketRequest::new(0, 0, 4);
        let range = context.bucket_range(&request).unwrap();
        assert_eq!(range, 0..2);
        let slice = context.bucket_slice(&request).unwrap();
        assert_eq!(slice, sorted_h64.as_slice());
        let sketches = context.bucket_sketches(&[request]).unwrap();
        assert_eq!(sketches.len(), 1);
        let mut roots = sketches[0].clone().decode_elements(4).unwrap();
        roots.sort_unstable();
        let mut expected_roots = sorted_h64;
        expected_roots.sort_unstable();
        assert_eq!(roots, expected_roots);
    }

    #[test]
    fn test_build_bucket_sketches_dynamic_summation() {
        use crate::triage::BucketRequest;
        let h1 = hash1();
        let h2 = hash2();

        // Depth 0 encompasses everything
        let requests = [BucketRequest::new(0, 0, 4)];

        let mut sorted_h64 = vec![h1.h64, h2.h64];
        sorted_h64.sort_unstable();
        let sketches = build_bucket_sketches(&sorted_h64, &requests).unwrap();

        assert_eq!(sketches.len(), 1);
        let mut roots = sketches[0].clone().decode_elements(4).unwrap();
        roots.sort_unstable();

        let mut expected = [h1.h64, h2.h64];
        expected.sort_unstable();

        assert_eq!(roots, expected);
    }

    #[test]
    fn test_build_bucket_sketches_deep_extraction() {
        use crate::triage::BucketRequest;
        let h1 = hash1();
        let h2 = hash2();

        let depth: u8 = 16;
        let shift = u32::from(H64_TRIE_WIDTH) - u32::from(depth);
        let prefix = h1.h64 >> shift;

        let requests = [BucketRequest::new(depth, prefix, 4)];

        // Deep extraction uses elements_provider
        let mut sorted_h64 = vec![h1.h64, h2.h64];
        sorted_h64.sort_unstable();
        let sketches = build_bucket_sketches(&sorted_h64, &requests).unwrap();

        assert_eq!(sketches.len(), 1);
        // Only elements that match the prefix should be present.
        let roots = sketches[0].clone().decode_elements(4).unwrap();
        assert!(roots.contains(&h1.h64));
    }

    #[test]
    fn test_build_bucket_sketches_invalid_indices() {
        use crate::triage::BucketRequest;
        let sorted_h64 = vec![];
        let requests = [BucketRequest::new(8, 256, 4)];
        assert_eq!(
            build_bucket_sketches(&sorted_h64, &requests),
            Err(AlgebraicError::InvalidBucketIndex)
        );

        let requests = [BucketRequest::new(7, 256, 4)];
        assert_eq!(
            build_bucket_sketches(&sorted_h64, &requests),
            Err(AlgebraicError::InvalidBucketIndex)
        );
    }

    #[test]
    fn test_build_bucket_sketches_depth_0_slow_path() {
        use crate::triage::BucketRequest;
        let h1 = hash1();

        let roots = single_bucket_roots(&[h1.h64], BucketRequest::new(0, 0, 10), 10);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], h1.h64);
    }

    #[test]
    fn test_sketch_builder_build_success_no_split() {
        use crate::triage::BucketRequest;
        let sorted_h64 = vec![0x1000_0000_0000_0000, 0x2000_0000_0000_0000];
        let index = H64Index::new(&sorted_h64);
        let builder = sketch_builder(&index, 1000, 1000);
        let requests = [BucketRequest::new(0, 0, 10)];

        let result = builder.build(&requests).unwrap();
        if let SketchResult::Success(sketches) = result {
            assert_eq!(sketches.len(), 1);
            assert_eq!(sketches[0].1, requests[0]);
        } else {
            panic!("Expected Success");
        }
    }

    #[test]
    fn test_sketch_builder_build_materializes_large_slice_at_small_capacity() {
        use crate::triage::BucketRequest;
        let sorted_h64 = vec![0x0000_0000_0000_0001, 0x8000_0000_0000_0002];
        let index = H64Index::new(&sorted_h64);
        let builder = sketch_builder(&index, 1000, 1000);
        let requests = [BucketRequest::new(0, 0, 1)];

        let result = builder.build(&requests).unwrap();
        assert!(matches!(result, SketchResult::Success(sketches) if sketches.len() == 1));
    }

    #[test]
    fn test_sketch_builder_build_hard_fallback() {
        use crate::triage::BucketRequest;
        let sorted_h64 = vec![1, 2, 3];
        let index = H64Index::new(&sorted_h64);
        // 3 > 2, triggers hard fallback
        let builder = sketch_builder(&index, 1000, 2);
        assert_fallback(&builder, &[BucketRequest::new(0, 0, 1)]);
    }

    #[test]
    fn test_sketch_builder_build_budget_exhausted() {
        use crate::triage::BucketRequest;
        // Two elements, capacity 1, will split.
        // Splitting creates two slices of length 1.
        // Aggregate work will be 1 + 1 = 2.
        let sorted_h64 = vec![0x0000_0000_0000_0001, 0x8000_0000_0000_0002];
        let index = H64Index::new(&sorted_h64);
        // Budget of 1 will be exhausted since 2 work is needed
        let builder = sketch_builder(&index, 1, 1000);
        assert_fallback(&builder, &[BucketRequest::new(0, 0, 1)]);
    }

    #[test]
    fn test_sketch_builder_build_falls_back_on_aggregate_work_limit() {
        use crate::triage::BucketRequest;

        let sorted_h64 = vec![0x1000_0000_0000_0000, 0x9000_0000_0000_0000];
        let index = H64Index::new(&sorted_h64);
        let builder = sketch_builder(&index, 1, 1000);
        assert_fallback(
            &builder,
            &[BucketRequest::new(1, 0, 1), BucketRequest::new(1, 1, 1)],
        );
    }

    #[test]
    fn test_sketch_builder_build_propagates_bucket_materialization_errors() {
        use crate::triage::BucketRequest;

        let sorted_h64 = vec![0, 0x1000_0000_0000_0000];
        let index = H64Index::new(&sorted_h64);
        let builder = sketch_builder(&index, 1000, 1000);
        let requests = [BucketRequest::new(0, 0, 2)];

        assert!(matches!(
            builder.build(&requests),
            Err(AlgebraicError::ZeroShortIdentifier)
        ));
    }

    #[test]
    fn test_sketch_builder_build_rejects_malformed_request_before_localization() {
        use crate::triage::BucketRequest;
        let sorted_h64 = vec![0x1000_0000_0000_0000, 0x2000_0000_0000_0000];
        let index = H64Index::new(&sorted_h64);
        let builder = sketch_builder(&index, 1000, 1000);
        // Zero capacity is rejected by `validate_bucket_requests` before any
        // range localization is attempted.
        let requests = [BucketRequest::new(0, 0, 0)];

        assert!(matches!(
            builder.build(&requests),
            Err(AlgebraicError::InvalidSketchCapacity)
        ));
    }

    #[test]
    fn test_sketch_builder_rejects_depth_above_maximum() {
        let sorted_h64 = [1_u64];
        let index = H64Index::new(&sorted_h64);
        let builder = sketch_builder(&index, 1_000, 1_000);
        let requests = [BucketRequest::new(crate::MAX_DEPTH.saturating_add(1), 0, 1)];

        assert!(matches!(
            builder.build(&requests),
            Err(AlgebraicError::InvalidBucketIndex)
        ));
    }

    const fn element(h128: u128, h64: u64) -> ElementHash {
        ElementHash { h128, h64, q3: h64 }
    }

    #[test]
    fn sorted_population_keeps_columns_aligned_and_ordered() {
        let population =
            SortedPopulation::new(vec![element(0xC, 30), element(0xA, 10), element(0xB, 20)]);
        assert_eq!(population.h64s(), &[10, 20, 30]);
        assert_eq!(population.h128s(), &[0xA, 0xB, 0xC]);
        assert_eq!(population.len(), 3);
        assert!(!population.is_empty());
    }

    #[test]
    fn sorted_population_candidates_are_multi_valued_and_deterministic() {
        let population = SortedPopulation::new(vec![
            element(0x9, 7),
            element(0x1, 7),
            element(0x5, 3),
            element(0x2, 9),
        ]);
        // Equal h64 values sort by h128, so the order is input-independent.
        assert_eq!(population.candidates(7), &[0x1, 0x9]);
        assert_eq!(population.candidates(3), &[0x5]);
        assert_eq!(population.candidates(8), []);
        assert_eq!(population.candidates(u64::MAX), []);
    }

    #[test]
    fn empty_sorted_population_has_no_candidates_and_empty_nodes() {
        let population = SortedPopulation::default();
        assert!(population.is_empty());
        assert_eq!(population.candidates(1), []);
        let nodes = build_bucket_nodes(&population, &[BucketRequest::new(0, 0, 4)]).unwrap();
        assert_eq!(nodes[0].1, NodeSummary::default());
    }

    #[test]
    fn bucket_nodes_summarize_exactly_the_slice_they_sketch() {
        let population = SortedPopulation::new(vec![
            element(0x1, 5),
            element(0x2, 6),
            element(0x4, (1_u64 << 63) | 5),
        ]);
        let nodes = build_bucket_nodes(
            &population,
            &[BucketRequest::new(1, 0, 4), BucketRequest::new(1, 1, 4)],
        )
        .unwrap();
        assert_eq!(
            nodes[0].1,
            NodeSummary {
                count: 2,
                digest: 0x3
            }
        );
        assert_eq!(
            nodes[1].1,
            NodeSummary {
                count: 1,
                digest: 0x4
            }
        );
        // The two children sum to the level-0 pair.
        let whole = build_bucket_nodes(&population, &[BucketRequest::new(0, 0, 4)]).unwrap();
        assert_eq!(
            whole[0].1,
            NodeSummary {
                count: 3,
                digest: 0x7
            }
        );
    }

    /// Room versions 1 and 2 have sender-chosen event IDs, so no `event_ids`
    /// frame may be built for them, and the rejection is explicit rather than
    /// a side effect of an ID that happens not to be valid base64.
    #[test]
    fn sender_chosen_rooms_cannot_construct_an_event_ids_frame() {
        let mut graph = MockGraph::new();
        graph.add_edge("$anchor", "$child");
        let anchors = [id("$anchor")];
        assert_eq!(
            compute_frame_digest(RoomEventIdKind::SenderChosen, &graph, &anchors),
            Err(AlgebraicError::UnsupportedRoomVersion)
        );
        let population = SortedPopulation::default();
        assert_eq!(
            ReconciliationContext::new(
                RoomEventIdKind::SenderChosen,
                &graph,
                &anchors,
                &population
            )
            .err(),
            Some(AlgebraicError::UnsupportedRoomVersion)
        );
    }

    /// A population that is not a `SortedPopulation`: it answers from an
    /// unordered list, as a persisted store reading runs would.
    struct ScanPopulation(Vec<ElementHash>);

    impl Population for ScanPopulation {
        fn node_summary(&self, node: (u8, u64)) -> Result<NodeSummary, AlgebraicError> {
            let range = H64Index::bounds_unchecked(&BucketRequest::new(node.0, node.1, 1));
            let h128s: Vec<u128> = self
                .0
                .iter()
                .filter(|e| range.contains(&u128::from(e.h64)))
                .map(|e| e.h128)
                .collect();
            NodeSummary::from_h128s(&h128s)
        }

        fn for_each_h64_in(&self, node: (u8, u64), f: &mut dyn FnMut(u64)) {
            let range = H64Index::bounds_unchecked(&BucketRequest::new(node.0, node.1, 1));
            let mut h64s: Vec<u64> = self
                .0
                .iter()
                .map(|e| e.h64)
                .filter(|h| range.contains(&u128::from(*h)))
                .collect();
            h64s.sort_unstable();
            h64s.into_iter().for_each(f);
        }

        fn candidates_into(&self, root: u64, out: &mut Vec<u128>) {
            let mut found: Vec<u128> = self
                .0
                .iter()
                .filter(|e| e.h64 == root)
                .map(|e| e.h128)
                .collect();
            found.sort_unstable();
            out.extend(found);
        }
    }

    #[test]
    fn build_bucket_nodes_serves_any_population_identically() {
        let elements = vec![
            element(0x9, 7),
            element(0x3, 7),
            element(0x5, 1 << 63),
            element(0x6, u64::MAX),
            element(0x1, 40),
        ];
        let sorted = SortedPopulation::new(elements.clone());
        let scan = ScanPopulation(elements);
        let requests = [
            BucketRequest::new(3, 0, 8),
            BucketRequest::new(2, 1, 8),
            BucketRequest::new(1, 1, 8),
        ];
        let nodes = build_bucket_nodes(&sorted, &requests).unwrap();
        assert_eq!(nodes, build_bucket_nodes(&scan, &requests).unwrap());
        assert_eq!(nodes[0].1.count, 3);
        assert_eq!(nodes[2].1.count, 2);
        let whole = [BucketRequest::new(0, 0, 8)];
        assert_eq!(
            build_bucket_nodes(&sorted, &whole).unwrap(),
            build_bucket_nodes(&scan, &whole).unwrap()
        );
    }

    #[test]
    fn candidates_into_appends_and_leaves_existing_entries() {
        let population =
            SortedPopulation::new(vec![element(0x9, 7), element(0x3, 7), element(0x4, 8)]);
        let mut out = vec![0xFF];
        population.candidates_into(7, &mut out);
        assert_eq!(out, [0xFF, 0x3, 0x9]);
        population.candidates_into(99, &mut out);
        assert_eq!(out, [0xFF, 0x3, 0x9]);
    }

    #[test]
    fn node_summary_matches_the_nodes_slice() {
        let population = SortedPopulation::new(vec![
            element(0x5, 1),
            element(0x6, 2),
            element(0x7, 1 << 63),
        ]);
        let root = population.node_summary((0, 0)).unwrap();
        assert_eq!(root.count, 3);
        assert_eq!(root.digest, 0x5 ^ 0x6 ^ 0x7);
        let right = population.node_summary((1, 1)).unwrap();
        assert_eq!((right.count, right.digest), (1, 0x7));
        assert_eq!(population.node_summary((1, 0)).unwrap().count, 2);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod incremental_tests {
    use super::*;
    use crate::test_util::digest32;
    use alloc::vec;

    fn elements(count: usize) -> Vec<FixedWindowElementHash> {
        (0..count)
            .map(|index| {
                FixedWindowElementHash::from_digest32(digest32(
                    u64::try_from(index).expect("test index fits u64"),
                ))
            })
            .collect()
    }

    fn assert_same(
        cached: &IncrementalFixedWindowPopulation,
        cold: &FixedWindowPopulation,
        requests: &[BucketRequest],
    ) {
        let cached_nodes = cached
            .bucket_nodes(requests)
            .unwrap_or_else(|error| panic!("cached failed with {error:?} for {requests:?}"));
        let cold_nodes = cold
            .bucket_nodes(requests)
            .unwrap_or_else(|error| panic!("cold failed with {error:?} for {requests:?}"));
        assert_eq!(
            cached_nodes, cold_nodes,
            "cached and cold nodes differ for {requests:?}"
        );
    }

    #[test]
    fn cached_ancestors_and_descendants_match_the_cold_population() {
        let all = elements(513);
        let cold = FixedWindowPopulation::new(all.clone()).unwrap();

        for cache_depth in [0_u8, 1, 4, DEFAULT_BUCKET_CACHE_DEPTH] {
            let cached = IncrementalFixedWindowPopulation::new(all.clone(), cache_depth).unwrap();
            assert_eq!(cached.len(), all.len());

            // Every prefix at or above the cached depth is answered by XORing
            // leaf sketches and summaries.
            for depth in 0..=cache_depth {
                let bucket_count = 1_u64 << depth;
                for capacity in [1_usize, 8, CACHED_LEAF_CAPACITY] {
                    let requests: Vec<_> = (0..bucket_count)
                        .map(|prefix| BucketRequest::new(depth, prefix, capacity))
                        .collect();
                    assert_same(&cached, &cold, &requests);
                }
            }

            // Deeper splits are answered by scanning only their leaf.
            for depth in (cache_depth + 1)..=crate::MAX_DEPTH.min(cache_depth + 3) {
                let bucket_count = 1_u64 << depth;
                let mut prefixes = alloc::vec![0, bucket_count / 2, bucket_count - 1];
                prefixes.sort_unstable();
                prefixes.dedup();
                let requests: Vec<_> = prefixes
                    .into_iter()
                    .map(|prefix| BucketRequest::new(depth, prefix, 8))
                    .collect();
                assert_same(&cached, &cold, &requests);
            }
        }
    }

    #[test]
    fn overflow_capacity_falls_back_to_the_exact_prefix_scan() {
        let all = elements(300);
        let cold = FixedWindowPopulation::new(all.clone()).unwrap();
        let cached =
            IncrementalFixedWindowPopulation::new(all, DEFAULT_BUCKET_CACHE_DEPTH).unwrap();
        for (depth, prefix) in [(0_u8, 0_u64), (3, 5), (7, 99)] {
            let request = BucketRequest::with_overflow(depth, prefix, 256);
            assert_same(&cached, &cold, &[request]);
        }
    }

    #[test]
    fn insert_and_remove_toggle_one_cached_leaf() {
        let all = elements(200);
        let mut cached = IncrementalFixedWindowPopulation::new(Vec::new(), 4).unwrap();
        for element in &all {
            cached.insert(*element).unwrap();
        }
        let cold = FixedWindowPopulation::new(all.clone()).unwrap();
        assert_same(
            &cached,
            &cold,
            &[BucketRequest::new(0, 0, CACHED_LEAF_CAPACITY)],
        );

        for element in &all[..80] {
            assert!(cached.remove(*element));
            assert!(!cached.remove(*element));
        }
        // Exercise the scan path after removal, not just the cached leaves.
        assert_same(
            &cached,
            &FixedWindowPopulation::new(all[80..].to_vec()).unwrap(),
            &[BucketRequest::with_overflow(0, 0, 256)],
        );
        let remaining = all[80..].to_vec();
        let cold = FixedWindowPopulation::new(remaining).unwrap();
        assert_eq!(cached.len(), all.len() - 80);
        assert_same(
            &cached,
            &cold,
            &[BucketRequest::new(0, 0, CACHED_LEAF_CAPACITY)],
        );

        for element in &all[..80] {
            cached.insert(*element).unwrap();
        }
        let cold = FixedWindowPopulation::new(all).unwrap();
        assert_same(
            &cached,
            &cold,
            &[BucketRequest::new(0, 0, CACHED_LEAF_CAPACITY)],
        );
    }

    #[test]
    fn deepest_prefix_includes_the_largest_bucket_key() {
        let element = FixedWindowElementHash::from_digest32([0xFF; 32]);
        let cold = FixedWindowPopulation::new(vec![element]).unwrap();
        let cached =
            IncrementalFixedWindowPopulation::new([element], DEFAULT_BUCKET_CACHE_DEPTH).unwrap();
        assert_same(
            &cached,
            &cold,
            &[BucketRequest::new(
                crate::MAX_DEPTH,
                (1_u64 << crate::MAX_DEPTH) - 1,
                CACHED_LEAF_CAPACITY,
            )],
        );
    }

    #[test]
    fn duplicate_elements_keep_their_multiplicity() {
        let element = FixedWindowElementHash::from_digest32(digest32(7));
        let mut cached = IncrementalFixedWindowPopulation::new([element, element], 3).unwrap();
        assert_eq!(cached.len(), 2);
        assert_same(
            &cached,
            &FixedWindowPopulation::new(vec![element, element]).unwrap(),
            &[BucketRequest::new(0, 0, CACHED_LEAF_CAPACITY)],
        );

        assert!(cached.remove(element));
        assert_eq!(cached.len(), 1);
        assert_same(
            &cached,
            &FixedWindowPopulation::new(vec![element]).unwrap(),
            &[BucketRequest::new(0, 0, CACHED_LEAF_CAPACITY)],
        );
        // An overflow request takes the scan path over `elements`, so a stale
        // multiplicity would show up here even though the cached leaf is right.
        assert_same(
            &cached,
            &FixedWindowPopulation::new(vec![element]).unwrap(),
            &[BucketRequest::with_overflow(0, 0, 256)],
        );

        assert!(cached.remove(element));
        assert!(cached.is_empty());
        assert_same(
            &cached,
            &FixedWindowPopulation::new(Vec::new()).unwrap(),
            &[BucketRequest::new(0, 0, CACHED_LEAF_CAPACITY)],
        );
        assert!(!cached.remove(element));
    }

    #[test]
    fn rejects_too_deep_caches_and_zero_elements() {
        assert_eq!(
            IncrementalFixedWindowPopulation::new(Vec::new(), DEFAULT_BUCKET_CACHE_DEPTH + 1),
            Err(AlgebraicError::InvalidBucketIndex)
        );
        let mut cached = IncrementalFixedWindowPopulation::new(Vec::new(), 3).unwrap();
        assert_eq!(
            cached.insert(FixedWindowElementHash::from_digest32([0; 32])),
            Err(AlgebraicError::ZeroSketchElement)
        );
    }
}
