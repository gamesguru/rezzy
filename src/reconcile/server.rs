// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Responder-side MSC0501 reconciliation digest generation.

use alloc::{
    collections::{BTreeSet, VecDeque},
    string::ToString,
};

use crate::basespec::rezzy_types::EventId;

use alloc::vec;
use alloc::vec::Vec;

use super::{
    AlgebraicError, ElementHash, EventIdFormat, RoomAccumulator, algebraic::SyndromeSketch,
    resident::ResidentKernel, triage::BucketRequest,
};

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

    /// Returns the event ID format for a given known event, used to compute
    /// its algebraic digest.
    ///
    /// Implementations MUST return the correct format even for rejected
    /// events (tombstones). If a rejected event's format is misidentified,
    /// its hash will be incorrect and will silently desync the reconciliation set.
    fn event_format(&self, id: &Id) -> EventIdFormat;
}

/// Computes the MSC0501 room digest over a negotiated frame.
///
/// The frame is mathematically bounded by the causal graph. The digested
/// population includes only the known events that **causally succeed** (are
/// topological descendants of) the anchor antichain. Events that causally
/// precede the anchor, such as pre-join history, are excluded.
///
/// # Errors
/// Returns an error if any of the descendant event IDs fail to hash properly.
pub fn compute_frame_digest<Id, Graph>(
    graph: &Graph,
    frame_event_ids: &[Id],
) -> Result<RoomAccumulator, AlgebraicError>
where
    Id: EventId,
    Graph: ForwardGraph<Id>,
{
    let mut accumulator = RoomAccumulator::new();
    let mut queue = VecDeque::new();
    let mut visited = BTreeSet::new();

    // Push frame anchor events as the starting boundary.
    for anchor in frame_event_ids {
        if graph.is_known(anchor) {
            queue.push_back(anchor.clone());
            // Mark anchors as visited so we do not re-traverse or digest them
            // if they are somehow referenced as children elsewhere.
            visited.insert(anchor.clone());
        }
    }

    // Breadth-first traversal down the causal graph
    while let Some(current) = queue.pop_front() {
        for child in graph.children(&current) {
            // Unconditionally mark visited to avoid re-traversing unknown forks
            if visited.insert(child.clone()) && graph.is_known(child) {
                // CAUTION: The graph implementation must correctly yield
                // the EventIdFormat even for rejected tombstones.
                let format = graph.event_format(child);
                let hash = ElementHash::from_matrix_event_id(&child.to_string(), format)?;

                accumulator.insert(hash)?;
                queue.push_back(child.clone());
            }
        }
    }

    Ok(accumulator)
}

/// Constructs bucket sketches for the provided MSC0501 triage requests.
///
/// Requests for shallow subsets (`depth <= 8` and `capacity <= 8`) are
/// synthesized dynamically from the pre-computed resident bounds in $O(1)$ time.
///
/// Requests for deeper partitions (`depth > 8` or `capacity > 8`) require
/// a scan of the causal frame (provided either by graph traversal or an explicitly
/// supplied element iterator).
///
/// # Errors
/// Returns an error if any sketches exceed capacity limits or graph traversal fails.
///
/// # Panics
/// Panics if the calculated prefix falls out of the bounds of a `u32` (should never happen for shifts <= 64).
pub fn build_bucket_sketches<I, F>(
    resident: &ResidentKernel,
    elements_provider: F,
    requests: &[BucketRequest],
) -> Result<Vec<SyndromeSketch>, AlgebraicError>
where
    I: Iterator<Item = ElementHash>,
    F: FnOnce() -> I,
{
    let mut sketches = Vec::with_capacity(requests.len());

    let requires_scan = requests.iter().any(|req| req.depth > 8 || req.capacity > 8);

    let mut sorted_index: Vec<u64> = Vec::new();
    if requires_scan {
        sorted_index.extend(elements_provider().map(|h| h.h64));
        sorted_index.sort_unstable();
    }

    for request in requests {
        if request.depth == 8 && request.capacity <= 8 {
            // Fast path: synthesize from ResidentKernel
            let bucket_index = request.prefix as usize;
            if bucket_index >= resident.buckets().len() {
                return Err(AlgebraicError::InvalidBucketIndex);
            }
            let bucket = &resident.buckets()[bucket_index];
            let coords = bucket.syndromes[..request.capacity].to_vec();
            sketches.push(SyndromeSketch::from_coordinates(coords)?);
        } else if request.depth <= 8 && request.capacity <= 8 {
            // Dynamic summation (XORing constituent buckets)
            let mut synthesized = vec![0_u64; request.capacity];
            let shift = 8_u8.saturating_sub(request.depth);
            let start_index = (request.prefix << shift) as usize;
            let end_index = start_index.saturating_add(1 << shift);

            if end_index > resident.buckets().len() {
                return Err(AlgebraicError::InvalidBucketIndex);
            }

            for bucket in &resident.buckets()[start_index..end_index] {
                for (i, coord) in synthesized.iter_mut().enumerate() {
                    *coord ^= bucket.syndromes[i];
                }
            }
            sketches.push(SyndromeSketch::from_coordinates(synthesized)?);
        } else {
            // Slow path: construct sketch from sorted index slice
            let mut sketch = SyndromeSketch::new(request.capacity)?;

            let shift = 64_u8.saturating_sub(request.depth);
            let start_h64 = if shift == 64 {
                0
            } else {
                u64::from(request.prefix) << shift
            };
            let end_h64_inclusive = if shift == 64 {
                u64::MAX
            } else {
                start_h64 | (1_u64 << shift).wrapping_sub(1)
            };

            let start_idx = sorted_index.partition_point(|&x| x < start_h64);
            let end_idx = sorted_index.partition_point(|&x| x <= end_h64_inclusive);

            for &h64 in &sorted_index[start_idx..end_idx] {
                sketch.toggle(h64)?;
            }

            sketches.push(sketch);
        }
    }

    Ok(sketches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt;

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
        fn new() -> Self {
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
            EventIdFormat::Legacy
        }
    }

    fn id(s: &str) -> MockId {
        MockId(s.to_string())
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

        let digest = compute_frame_digest(&graph, &[id("$anchor")]).unwrap();

        let mut expected = RoomAccumulator::new();
        expected
            .insert(ElementHash::from_matrix_event_id("$child1", EventIdFormat::Legacy).unwrap())
            .unwrap();
        expected
            .insert(ElementHash::from_matrix_event_id("$child2", EventIdFormat::Legacy).unwrap())
            .unwrap();
        expected
            .insert(
                ElementHash::from_matrix_event_id("$grandchild", EventIdFormat::Legacy).unwrap(),
            )
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

        let digest = compute_frame_digest(&graph, &[id("$anchor")]).unwrap();

        let mut expected = RoomAccumulator::new();
        expected
            .insert(ElementHash::from_matrix_event_id("$child", EventIdFormat::Legacy).unwrap())
            .unwrap();

        assert_eq!(digest.digest(), expected.digest());
    }

    #[test]
    fn test_build_bucket_sketches_exact_match() {
        use crate::reconcile::triage::BucketRequest;
        let mut resident = ResidentKernel::new();
        let h1 = ElementHash::from_matrix_event_id("$1", EventIdFormat::Legacy).unwrap();
        resident.insert(h1).unwrap();

        let bucket_idx = (h1.h64 >> 56) as u32;
        let requests = [BucketRequest {
            depth: 8,
            prefix: bucket_idx,
            capacity: 4,
        }];

        let sketches = build_bucket_sketches(&resident, core::iter::empty, &requests).unwrap();

        assert_eq!(sketches.len(), 1);
        let roots = sketches[0].clone().decode_elements(4).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], h1.h64);
    }

    #[test]
    fn test_build_bucket_sketches_dynamic_summation() {
        use crate::reconcile::triage::BucketRequest;
        let mut resident = ResidentKernel::new();
        let h1 = ElementHash::from_matrix_event_id("$1", EventIdFormat::Legacy).unwrap();
        let h2 = ElementHash::from_matrix_event_id("$2", EventIdFormat::Legacy).unwrap();
        resident.insert(h1).unwrap();
        resident.insert(h2).unwrap();

        // Depth 0 encompasses everything
        let requests = [BucketRequest {
            depth: 0,
            prefix: 0,
            capacity: 4,
        }];

        let sketches = build_bucket_sketches(&resident, core::iter::empty, &requests).unwrap();

        assert_eq!(sketches.len(), 1);
        let mut roots = sketches[0].clone().decode_elements(4).unwrap();
        roots.sort_unstable();

        let mut expected = [h1.h64, h2.h64];
        expected.sort_unstable();

        assert_eq!(roots, expected);
    }

    #[test]
    fn test_build_bucket_sketches_deep_extraction() {
        use crate::reconcile::triage::BucketRequest;
        let mut resident = ResidentKernel::new();
        let h1 = ElementHash::from_matrix_event_id("$1", EventIdFormat::Legacy).unwrap();
        let h2 = ElementHash::from_matrix_event_id("$2", EventIdFormat::Legacy).unwrap();
        resident.insert(h1).unwrap();
        resident.insert(h2).unwrap();

        let depth = 16;
        let shift = 64 - depth;
        let prefix = u32::try_from(h1.h64 >> shift).unwrap();

        let requests = [BucketRequest {
            depth,
            prefix,
            capacity: 4,
        }];

        // Deep extraction uses elements_provider
        let elements = vec![h1, h2];
        let sketches =
            build_bucket_sketches(&resident, || elements.into_iter(), &requests).unwrap();

        assert_eq!(sketches.len(), 1);
        // Only elements that match the prefix should be present.
        let roots = sketches[0].clone().decode_elements(4).unwrap();
        assert!(roots.contains(&h1.h64));
    }

    #[test]
    fn test_build_bucket_sketches_invalid_indices() {
        use crate::reconcile::triage::BucketRequest;
        let resident = ResidentKernel::new();
        
        let requests = [BucketRequest {
            depth: 8,
            prefix: 256, // out of bounds for resident.buckets().len() == 256
            capacity: 4,
        }];
        assert_eq!(
            build_bucket_sketches(&resident, core::iter::empty, &requests),
            Err(AlgebraicError::InvalidBucketIndex)
        );

        let requests = [BucketRequest {
            depth: 7,
            prefix: 256, // start_index = 512, out of bounds
            capacity: 4,
        }];
        assert_eq!(
            build_bucket_sketches(&resident, core::iter::empty, &requests),
            Err(AlgebraicError::InvalidBucketIndex)
        );
    }

    #[test]
    fn test_build_bucket_sketches_depth_0_slow_path() {
        use crate::reconcile::triage::BucketRequest;
        let mut resident = ResidentKernel::new();
        let h1 = ElementHash::from_matrix_event_id("$1", EventIdFormat::Legacy).unwrap();
        resident.insert(h1).unwrap();

        let requests = [BucketRequest {
            depth: 0,
            prefix: 0,
            capacity: 10, // > 8 forces the slow path
        }];

        let elements = vec![h1];
        let sketches = build_bucket_sketches(&resident, || elements.into_iter(), &requests).unwrap();

        assert_eq!(sketches.len(), 1);
        let roots = sketches[0].clone().decode_elements(10).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], h1.h64);
    }
}
