//! Pure reachability contract for room DAG accelerators.
//!
//! This module intentionally stays free of storage, threading, and cache
//! policy. It defines only the query result type and the minimal trait that a
//! drop-in accelerator must satisfy.

use crate::basespec::rezzy_types::LeanEvent;
use crate::HashMap;
use alloc::vec;
use alloc::vec::Vec;
use roaring::RoaringBitmap;

/// Tri-state reachability answer.
///
/// `Unknown` is a valid, non-error result. Callers use it to fall back to the
/// always-correct slow path when the accelerator cannot prove `Yes` or `No`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reach {
    Yes,
    No,
    Unknown,
}

impl Reach {
    /// Returns `true` when the answer is definitive.
    #[inline]
    #[must_use]
    pub const fn is_definitive(self) -> bool {
        matches!(self, Self::Yes | Self::No)
    }

    /// Returns `true` when the accelerator proved reachability.
    #[inline]
    #[must_use]
    pub const fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }

    /// Returns `true` when the accelerator proved non-reachability.
    #[inline]
    #[must_use]
    pub const fn is_no(self) -> bool {
        matches!(self, Self::No)
    }

    /// Returns `true` when the caller should consult the slow path.
    #[inline]
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// Minimal contract for a reachability accelerator.
///
/// Implementations may use live overlays, sealed segments, bridge sets, or any
/// other indexing strategy. The only requirement is that `Unknown` must be a
/// safe fallback, never a correctness failure.
pub trait Reachability {
    /// Event identifier type used by the accelerator.
    type Id: ?Sized;

    /// Returns whether `from` can reach `to`.
    ///
    /// The contract is intentionally asymmetric:
    /// - `Reach::Yes` and `Reach::No` are hard answers.
    /// - `Reach::Unknown` means "ask the slow path."
    fn reaches(&self, from: &Self::Id, to: &Self::Id) -> Reach;

    /// Batch filter for the common antichain-to-candidate case.
    ///
    /// The default implementation is an optimistic yes-only filter: it linearly
    /// scans the candidates and keeps only the indices for which at least one
    /// seed definitively proves `Reach::Yes`.
    ///
    /// Callers must treat omitted candidates, including `Reach::No` and
    /// `Reach::Unknown` results, as slow-path fallbacks and send them to the
    /// always-correct resolver.
    ///
    /// Override implementations must preserve that yes-only contract.
    #[must_use]
    fn filter_reachable<'a, S, C>(&self, seeds: S, candidates: C) -> Vec<usize>
    where
        S: IntoIterator<Item = &'a Self::Id>,
        C: IntoIterator<Item = &'a Self::Id>,
        Self::Id: 'a,
    {
        let seeds: Vec<&'a Self::Id> = seeds.into_iter().collect();
        candidates
            .into_iter()
            .enumerate()
            .filter_map(|(idx, candidate)| {
                seeds
                    .iter()
                    .copied()
                    .any(|seed| self.reaches(seed, candidate).is_yes())
                    .then_some(idx)
            })
            .collect()
    }
}

/// Forward reachability accelerator over a topologically ordered DAG snapshot.
///
/// The index stores, for each node, the transitive closure of its descendants
/// as a compressed bitmap. This makes repeated "which candidates are
/// forward-reachable from these seeds?" queries fast: seed closures are `ORed`
/// once, then candidate membership is a bitmap lookup.
#[derive(Debug, Clone)]
pub struct ForwardReachabilityIndex<Id> {
    id_to_index: HashMap<Id, u32>,
    descendant_bitmaps: Vec<RoaringBitmap>,
}

impl<Id> ForwardReachabilityIndex<Id>
where
    Id: crate::basespec::rezzy_types::EventId + Ord,
{
    /// Builds the forward reachability index from a DAG snapshot.
    ///
    /// The input graph must be acyclic with edges expressed through
    /// `auth_events`.
    ///
    /// # Panics
    /// Panics if the input graph is internally inconsistent or contains a
    /// cycle that prevents the topological build from completing.
    #[must_use]
    pub fn build<C: Clone, S: core::hash::BuildHasher>(
        graph: &HashMap<Id, LeanEvent<Id, C>, S>,
    ) -> Self {
        let mut in_degree: HashMap<&Id, usize> = HashMap::new();
        let mut children: HashMap<&Id, Vec<&Id>> = HashMap::new();

        for (id, ev) in graph {
            in_degree.entry(id).or_insert(0);
            for auth_id in &ev.auth_events {
                if graph.contains_key(auth_id) {
                    children.entry(auth_id).or_default().push(id);
                    let degree = in_degree.entry(id).or_insert(0);
                    *degree = degree.saturating_add(1);
                }
            }
        }

        let mut queue: Vec<&Id> = in_degree
            .iter()
            .filter_map(|(id, &deg)| (deg == 0).then_some(*id))
            .collect();
        queue.sort_unstable();

        let mut topo = Vec::with_capacity(graph.len());
        let mut head = 0_usize;
        while head < queue.len() {
            let id = queue[head];
            head = head.saturating_add(1);
            topo.push(id);
            if let Some(children) = children.get(id) {
                for child in children {
                    let degree = in_degree.get_mut(child).unwrap();
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        queue.push(child);
                    }
                }
            }
        }

        debug_assert_eq!(
            topo.len(),
            graph.len(),
            "forward reachability graph must be acyclic"
        );

        let mut id_to_index = HashMap::with_capacity(topo.len());
        for (idx, &id) in topo.iter().enumerate() {
            let idx = u32::try_from(idx).expect("graph too large for roaring bitmap index");
            id_to_index.insert(id.clone(), idx);
        }

        let mut children_by_index = vec![Vec::<u32>::new(); topo.len()];
        for (parent_id, child_ids) in children {
            let Some(&parent_idx) = id_to_index.get(parent_id) else {
                continue;
            };
            let parent_slot = &mut children_by_index[parent_idx as usize];
            for child_id in child_ids {
                if let Some(&child_idx) = id_to_index.get(child_id) {
                    parent_slot.push(child_idx);
                }
            }
        }

        let mut descendant_bitmaps = vec![RoaringBitmap::new(); topo.len()];
        for idx in (0..topo.len()).rev() {
            let mut bitmap = RoaringBitmap::new();
            bitmap.insert(u32::try_from(idx).expect("graph too large for roaring bitmap index"));
            for &child_idx in &children_by_index[idx] {
                bitmap |= &descendant_bitmaps[child_idx as usize];
            }
            descendant_bitmaps[idx] = bitmap;
        }

        Self {
            id_to_index,
            descendant_bitmaps,
        }
    }

    /// Returns the descendants of a seed set as candidate indices.
    ///
    /// Callers must interpret the returned indices relative to the candidate
    /// iteration order they provided.
    #[must_use]
    pub fn filter_reachable<'a, S, C>(&self, seeds: S, candidates: C) -> Vec<usize>
    where
        S: IntoIterator<Item = &'a Id>,
        C: IntoIterator<Item = &'a Id>,
        Id: 'a,
    {
        let mut reachable = RoaringBitmap::new();
        for seed in seeds {
            let Some(&idx) = self.id_to_index.get(seed) else {
                continue;
            };
            reachable.insert(idx);
            reachable |= &self.descendant_bitmaps[idx as usize];
        }

        candidates
            .into_iter()
            .enumerate()
            .filter_map(|(idx, candidate)| {
                self.id_to_index
                    .get(candidate)
                    .copied()
                    .filter(|candidate_idx| reachable.contains(*candidate_idx))
                    .map(|_| idx)
            })
            .collect()
    }
}

impl<Id> Reachability for ForwardReachabilityIndex<Id>
where
    Id: crate::basespec::rezzy_types::EventId + Ord,
{
    type Id = Id;

    fn reaches(&self, from: &Self::Id, to: &Self::Id) -> Reach {
        let Some(&from_idx) = self.id_to_index.get(from) else {
            return Reach::Unknown;
        };
        let Some(&to_idx) = self.id_to_index.get(to) else {
            return Reach::Unknown;
        };
        if from_idx == to_idx {
            return Reach::Yes;
        }
        if self.descendant_bitmaps[from_idx as usize].contains(to_idx) {
            Reach::Yes
        } else {
            Reach::No
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::basespec::rezzy_types::LeanEvent;
    use crate::HashMap;
    use alloc::string::String;
    use alloc::vec;

    struct Dummy;

    impl Reachability for Dummy {
        type Id = u32;

        fn reaches(&self, from: &Self::Id, to: &Self::Id) -> Reach {
            if from == to {
                Reach::Yes
            } else {
                Reach::Unknown
            }
        }
    }

    #[test]
    fn reach_helpers_reflect_the_variant() {
        assert!(Reach::Yes.is_definitive());
        assert!(Reach::No.is_definitive());
        assert!(!Reach::Unknown.is_definitive());
        assert!(Reach::Yes.is_yes());
        assert!(Reach::No.is_no());
        assert!(Reach::Unknown.is_unknown());
    }

    #[test]
    fn trait_contract_allows_unknown_fallback() {
        let accel = Dummy;
        assert_eq!(accel.reaches(&7, &7), Reach::Yes);
        assert_eq!(accel.reaches(&7, &8), Reach::Unknown);
    }

    #[test]
    fn batch_filter_defaults_to_any_reachable_candidate() {
        let accel = Dummy;
        let seeds = [&1_u32, &3_u32];
        let candidates = [&2_u32, &3_u32, &4_u32];
        assert_eq!(accel.filter_reachable(seeds, candidates), vec![1]);
    }

    #[test]
    fn forward_reachability_index_builds_and_queries_descendants() {
        let mut graph: HashMap<String, LeanEvent<String>> = HashMap::new();
        let a = String::from("A");
        let b = String::from("B");
        let c = String::from("C");
        let missing = String::from("missing");
        graph.insert(
            a.clone(),
            LeanEvent {
                event_id: a.clone(),
                auth_events: vec![],
                ..Default::default()
            },
        );
        graph.insert(
            b.clone(),
            LeanEvent {
                event_id: b.clone(),
                auth_events: vec![a.clone()],
                ..Default::default()
            },
        );
        graph.insert(
            c.clone(),
            LeanEvent {
                event_id: c.clone(),
                auth_events: vec![b.clone()],
                ..Default::default()
            },
        );

        let index = ForwardReachabilityIndex::build(&graph);

        assert_eq!(index.reaches(&a, &c), Reach::Yes);
        assert_eq!(index.reaches(&c, &a), Reach::No);
        assert_eq!(index.reaches(&a, &a), Reach::Yes);
        assert_eq!(index.reaches(&a, &missing), Reach::Unknown);

        let seeds = [&a];
        let candidates = [&a, &b, &c];
        assert_eq!(index.filter_reachable(seeds, candidates), vec![0, 1, 2]);
    }
}
