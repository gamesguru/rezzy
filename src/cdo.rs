use crate::types::LeanEvent;
use crate::HashMap;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

#[must_use]
pub fn is_ancestor<S: core::hash::BuildHasher>(
    child_id: &str,
    possible_ancestor_id: &str,
    context: &HashMap<String, LeanEvent, S>,
) -> bool {
    if child_id == possible_ancestor_id {
        return true;
    }
    let Some(child_ev) = context.get(child_id) else {
        return false;
    };
    let Some(and_ev) = context.get(possible_ancestor_id) else {
        return false;
    };

    // Only apply depth pruning if depths are populated (greater than 0).
    // Test events created with Default::default() default to depth 0.
    if child_ev.depth > 0 && and_ev.depth > 0 && and_ev.depth >= child_ev.depth {
        return false;
    }

    let mut stack = Vec::new();
    stack.push(child_id);
    let mut visited = BTreeSet::new();
    visited.insert(child_id);

    while let Some(current) = stack.pop() {
        if current == possible_ancestor_id {
            return true;
        }
        if let Some(ev) = context.get(current) {
            // Prune branches that are already at or below the ancestor's depth (if populated)
            if ev.depth > 0 && and_ev.depth > 0 && ev.depth <= and_ev.depth {
                continue;
            }
            for parent in ev.prev_events.iter().chain(ev.auth_events.iter()) {
                if visited.insert(parent.as_str()) {
                    stack.push(parent.as_str());
                }
            }
        }
    }
    false
}

#[allow(clippy::manual_div_ceil, clippy::needless_range_loop)]
fn compute_cdo_bit_masks_unbounded<'a, S: core::hash::BuildHasher>(
    dag_context: &'a HashMap<String, LeanEvent, S>,
    admin_actions: &[&'a str],
) -> (Vec<u64>, Vec<u64>, HashMap<&'a str, usize>) {
    let n = dag_context.len();
    let num_words = admin_actions.len().div_ceil(64);

    // 1. Assign O(1) integer indices and sort topologically by depth
    let mut id_to_idx = HashMap::with_capacity(n);
    let mut sorted_events = Vec::with_capacity(n);

    for (id, ev) in dag_context {
        let idx = id_to_idx.len();
        id_to_idx.insert(id.as_str(), idx);
        sorted_events.push((idx, ev));
    }
    // Depth-ascending gives us a free topological sort!
    sorted_events.sort_unstable_by_key(|&(_, ev)| ev.depth);

    // 2. Build integer-based adjacency lists
    let mut parents: Vec<Vec<usize>> = alloc::vec![Vec::new(); n];
    let mut children: Vec<Vec<usize>> = alloc::vec![Vec::new(); n];
    for &(u, ev) in &sorted_events {
        for p_id in ev.prev_events.iter().chain(ev.auth_events.iter()) {
            if let Some(&v) = id_to_idx.get(p_id.as_str()) {
                parents[u].push(v);
                children[v].push(u);
            }
        }
    }

    // 3. Flat 1D Arrays (Zero inner heap allocations)
    let mut and_masks = alloc::vec![0u64; n * num_words];
    let mut desc_masks = alloc::vec![0u64; n * num_words];

    for (i, &admin_id) in admin_actions.iter().enumerate() {
        if let Some(&idx) = id_to_idx.get(admin_id) {
            let word = i / 64;
            let bit = 1u64 << (i % 64);
            and_masks[idx * num_words + word] |= bit;
            desc_masks[idx * num_words + word] |= bit;
        }
    }

    // 4. Forward Sweep (Ancestors) - Pure array iteration
    for &(u, _) in &sorted_events {
        let u_base = u * num_words;
        for &p in &parents[u] {
            let p_base = p * num_words;
            for w in 0..num_words {
                and_masks[u_base + w] |= and_masks[p_base + w];
            }
        }
    }

    // 5. Backward Sweep (Descendants) - Pure array iteration
    for &(u, _) in sorted_events.iter().rev() {
        let u_base = u * num_words;
        for &c in &children[u] {
            let c_base = c * num_words;
            for w in 0..num_words {
                desc_masks[u_base + w] |= desc_masks[c_base + w];
            }
        }
    }

    (and_masks, desc_masks, id_to_idx)
}

fn sort_cdo_events(events: &[&LeanEvent]) -> Vec<LeanEvent> {
    let mut sorted = events.iter().copied().cloned().collect::<Vec<_>>();
    sorted.sort_by(|a, b| {
        let type_priority = |t: &str| match t {
            "m.room.power_levels" => 0,
            "m.room.join_rules" => 1,
            _ => 2,
        };

        let cmp_pl = b.power_level.cmp(&a.power_level);
        if cmp_pl != Ordering::Equal {
            return cmp_pl;
        }

        let cmp_type = type_priority(&a.event_type).cmp(&type_priority(&b.event_type));
        if cmp_type != Ordering::Equal {
            return cmp_type;
        }

        let cmp_ts = a.origin_server_ts.cmp(&b.origin_server_ts);
        if cmp_ts != Ordering::Equal {
            return cmp_ts;
        }

        a.event_id.cmp(&b.event_id)
    });
    sorted
}

/// Cycle 0 Topological Filter: Vectorized Causal Domination Operator (CDO)
/// Executes strictly on the Conflicted State Subgraph (C).
#[must_use]
pub fn apply_cdo_filter<S1: core::hash::BuildHasher, S2: core::hash::BuildHasher>(
    conflicted_events: &HashMap<String, LeanEvent, S1>,
    auth_context: &HashMap<String, LeanEvent, S2>,
) -> HashMap<String, LeanEvent> {
    // Build sort/DAG context to determine ancestries
    let dag_context: HashMap<String, LeanEvent> = auth_context
        .iter()
        .chain(conflicted_events.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Identify all admin actions in conflicted events
    let admin_actions: Vec<&str> = conflicted_events
        .values()
        .filter(|e| e.is_ban_or_kick() || e.is_demotion() || e.is_lockdown())
        .map(|e| e.event_id.as_str())
        .collect();

    let num_words = admin_actions.len().div_ceil(64);

    let (and_masks, desc_masks, id_to_idx) =
        compute_cdo_bit_masks_unbounded(&dag_context, &admin_actions);

    let sorted_events = sort_cdo_events(&conflicted_events.values().collect::<Vec<_>>());

    let mut dropped_ids = BTreeSet::new();
    let mut active_admin_actions: Vec<&LeanEvent> = Vec::new();

    // Build map from admin_id to its index in admin_actions
    let mut admin_to_pos = HashMap::new();
    for (i, &admin_id) in admin_actions.iter().enumerate() {
        admin_to_pos.insert(admin_id, i);
    }

    // Pass 2: Direct Domination (Sender / Type Restriction) in priority order
    for event in &sorted_events {
        let mut is_dominated = false;
        let event_id = event.event_id.as_str();

        if let Some(&ev_idx) = id_to_idx.get(event_id) {
            for admin_ev in &active_admin_actions {
                if admin_ev.restricts_event(event) {
                    let admin_id = admin_ev.event_id.as_str();
                    if let Some(&orig_idx) = admin_to_pos.get(admin_id) {
                        let word = orig_idx / 64;
                        let bit = 1u64 << (orig_idx % 64);

                        let is_ancestor_admin = (and_masks[ev_idx * num_words + word] & bit) != 0;
                        let is_descendant_admin =
                            (desc_masks[ev_idx * num_words + word] & bit) != 0;

                        if !is_ancestor_admin && !is_descendant_admin {
                            is_dominated = true;
                            break;
                        }
                    }
                }
            }
        }

        if is_dominated {
            dropped_ids.insert(event.event_id.clone());
        } else if event.is_ban_or_kick() || event.is_demotion() || event.is_lockdown() {
            // Event survived and is an admin action, so it is active for subsequent events
            active_admin_actions.push(event);
        }
    }

    // Pass 3: Auth-Dependency Domination (Transitive Closure / Linear-Time propagation)
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    for (id, event) in conflicted_events {
        for auth_id in &event.auth_events {
            dependents
                .entry(auth_id.clone())
                .or_default()
                .push(id.clone());
        }
    }

    let mut queue: Vec<String> = dropped_ids.iter().cloned().collect();
    while let Some(current_dropped) = queue.pop() {
        if let Some(children) = dependents.get(&current_dropped) {
            for child in children {
                if !dropped_ids.contains(child) {
                    dropped_ids.insert(child.clone());
                    queue.push(child.clone());
                }
            }
        }
    }

    // Return strictly the transitively safe set
    let mut safe_set = HashMap::new();
    for (id, event) in conflicted_events {
        if !dropped_ids.contains(id) {
            safe_set.insert(id.clone(), event.clone());
        }
    }

    safe_set
}
