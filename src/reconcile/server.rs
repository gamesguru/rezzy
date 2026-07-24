// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Responder-side MSC0501 reconciliation digest generation.

use alloc::{collections::BTreeSet, string::ToString, vec::Vec};

use crate::basespec::rezzy_types::EventId;

use super::{AlgebraicError, ElementHash, EventIdFormat, RoomAccumulator};

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
    let mut queue = Vec::new();
    let mut visited = BTreeSet::new();

    // Push frame anchor events as the starting boundary.
    for anchor in frame_event_ids {
        if graph.is_known(anchor) {
            queue.push(anchor.clone());
        }
    }

    // Breadth-first traversal down the causal graph
    while let Some(current) = queue.pop() {
        for child in graph.children(&current) {
            // Only process each child once to avoid combinatorial explosions on forks
            if visited.insert(child.clone()) && graph.is_known(child) {
                let format = graph.event_format(child);
                let hash = ElementHash::from_matrix_event_id(&child.to_string(), format)?;
                accumulator.insert(hash)?;
                queue.push(child.clone());
            }
        }
    }

    Ok(accumulator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::string::String;
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
}
