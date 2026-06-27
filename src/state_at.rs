use crate::types::{LeanEvent, StateResVersion};
use crate::HashMap;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

pub type LocalAuthCache = HashMap<String, BTreeMap<(String, Option<String>), (LeanEvent, usize)>>;

pub(crate) struct OverlayState<'a, S1, S2> {
    pub(crate) resolved: &'a BTreeMap<(String, Option<String>), String>,
    pub(crate) auth_context: &'a HashMap<String, LeanEvent, S1>,
    pub(crate) conflicted: &'a HashMap<String, LeanEvent, S2>,
    pub(crate) local_auth: BTreeMap<(String, Option<String>), LeanEvent>,
    pub(crate) create_ev: Option<&'a LeanEvent>,
    pub(crate) version: StateResVersion,
    pub(crate) is_power_phase: bool,
}

impl<S1: core::hash::BuildHasher, S2: core::hash::BuildHasher> crate::auth::StateProvider
    for OverlayState<'_, S1, S2>
{
    fn get_event(&self, event_type: &str, state_key: Option<&str>) -> Option<&LeanEvent> {
        let query: &dyn crate::auth::StateKeyDyn = &(event_type, state_key);

        // In V2.1 (Stock MSC4297), we supplement with ONLY m.room.power_levels during Step 2 (power phase).
        // In Step 4 (remaining events phase), we supplement with all event types.
        let should_supplement = match self.version {
            StateResVersion::V2_1 => {
                if self.is_power_phase {
                    event_type == "m.room.power_levels" && state_key == Some("")
                } else {
                    true
                }
            }
            StateResVersion::V2_1_1 => {
                (event_type == "m.room.power_levels" && state_key == Some(""))
                    || (event_type == "m.room.member")
            }
            _ => true,
        };

        if should_supplement {
            // Check consensus resolved state
            if let Some(eid) = self.resolved.get(query) {
                if let Some(ev) = self
                    .auth_context
                    .get(eid)
                    .or_else(|| self.conflicted.get(eid))
                {
                    if self.version == StateResVersion::V2_1_1 && event_type == "m.room.member" {
                        // V2.1.1 Fix: Only supplement bans and kicks
                        if let Some(membership) =
                            ev.content.get("membership").and_then(|m| m.as_str())
                        {
                            let is_ban = membership == "ban";
                            let is_kick =
                                membership == "leave" && Some(ev.sender.as_str()) != state_key;
                            if is_ban || is_kick {
                                return Some(ev);
                            }
                        }
                        // If it's a normal join/invite, fall through to local auth
                    } else {
                        return Some(ev);
                    }
                }
            }
        }

        // Check local auth chain (BFS result)
        if let Some(ev) = self.local_auth.get(query) {
            return Some(ev);
        }
        // Fallback for create
        if event_type == "m.room.create" && state_key == Some("") {
            return self.create_ev;
        }
        None
    }
}

/// Evaluates whether an event passes authentication checks given a resolved state map,
/// delegating to the core `crate::auth::check_auth` logic via a temporary `OverlayState` view.
#[allow(clippy::too_many_arguments)]
pub(crate) fn iterative_auth_ok<S1: core::hash::BuildHasher, S2: core::hash::BuildHasher>(
    event: &LeanEvent,
    resolved: &BTreeMap<(String, Option<String>), String>,
    auth_context: &HashMap<String, LeanEvent, S1>,
    conflicted_events: &HashMap<String, LeanEvent, S2>,
    local_auth: BTreeMap<(String, Option<String>), LeanEvent>,
    cached_create: Option<&LeanEvent>,
    version: StateResVersion,
    is_power_phase: bool,
) -> bool {
    let overlay = OverlayState {
        resolved,
        auth_context,
        conflicted: conflicted_events,
        local_auth,
        create_ev: cached_create,
        version,
        is_power_phase,
    };

    crate::auth::check_auth(event, &overlay).is_ok()
}

pub(crate) fn update_local_auth(
    local_auth: &mut BTreeMap<(String, Option<String>), (LeanEvent, usize)>,
    aev: &LeanEvent,
    current_depth: usize,
) {
    let key = (aev.event_type.clone(), aev.state_key.clone());
    match local_auth.entry(key) {
        alloc::collections::btree_map::Entry::Vacant(e) => {
            e.insert((aev.clone(), current_depth));
        }
        alloc::collections::btree_map::Entry::Occupied(mut e) => {
            if current_depth < e.get().1 {
                e.insert((aev.clone(), current_depth));
            }
        }
    }
}

/// Recursively compute the local auth context for an event, using memoization
/// to avoid redundant graph walks. The context is represented as a map of
/// (type, `state_key`) -> (`LeanEvent`, depth), ensuring that for each key, the "closest"
/// auth event in the chain is preserved (shortest path).
pub(crate) fn compute_local_auth<S1: core::hash::BuildHasher, S2: core::hash::BuildHasher>(
    event: &LeanEvent,
    auth_context: &HashMap<String, LeanEvent, S1>,
    conflicted_events: &HashMap<String, LeanEvent, S2>,
    cache: &mut LocalAuthCache,
    version: StateResVersion,
) -> BTreeMap<(String, Option<String>), LeanEvent> {
    if let Some(cached) = cache.get(&event.event_id) {
        return cached
            .clone()
            .into_iter()
            .map(|(k, (v, _))| (k, v))
            .collect();
    }

    let mut local_auth: BTreeMap<(String, Option<String>), (LeanEvent, usize)> = BTreeMap::new();
    let mut queue = alloc::collections::VecDeque::new();
    for aid in &event.auth_events {
        queue.push_back((aid.clone(), 1));
    }
    let mut visited = BTreeSet::new();

    while let Some((aid, current_depth)) = queue.pop_front() {
        if !visited.insert(aid.clone()) {
            continue;
        }

        if let Some(cached_ancestor) = cache.get(&aid) {
            // The cache only contains the parents of `aid`. We must also insert `aid` itself!
            if let Some(aev) = auth_context
                .get(&aid)
                .or_else(|| conflicted_events.get(&aid))
            {
                update_local_auth(&mut local_auth, aev, current_depth);
            }

            for (key, (ev, cached_depth)) in cached_ancestor {
                let total_depth = current_depth + cached_depth;
                match local_auth.entry(key.clone()) {
                    alloc::collections::btree_map::Entry::Vacant(e) => {
                        e.insert((ev.clone(), total_depth));
                    }
                    alloc::collections::btree_map::Entry::Occupied(mut e) => {
                        if total_depth < e.get().1 {
                            e.insert((ev.clone(), total_depth));
                        }
                    }
                }
            }
            continue;
        }

        if let Some(aev) = auth_context
            .get(&aid)
            .or_else(|| conflicted_events.get(&aid))
        {
            update_local_auth(&mut local_auth, aev, current_depth);

            // Recursive traversal is NEW in V2.2.
            // For V2.1 and below, we only check the immediate auth_events.
            if version == StateResVersion::V2_2 {
                for parent_id in &aev.auth_events {
                    queue.push_back((parent_id.clone(), current_depth + 1));
                }
            }
        }
    }

    cache.insert(event.event_id.clone(), local_auth.clone());
    local_auth.into_iter().map(|(k, (v, _))| (k, v)).collect()
}

/// Computes the state map at (after) a given target event ID,
/// assuming all ancestral events are present in `events_map`.
///
/// # Panics
///
/// Will panic if graph invariants are violated (specifically, if an ancestor event
/// present in the reachable subgraph is missing from `events_map` during topological processing).
#[must_use]
pub fn compute_state_at<S: core::hash::BuildHasher>(
    target_event_id: &str,
    events_map: &HashMap<String, LeanEvent, S>,
) -> Option<BTreeMap<(String, Option<String>), String>> {
    if !events_map.contains_key(target_event_id) {
        return None;
    }

    // 1. Backward walk to find causal history (prev_events)
    let mut visited = BTreeSet::new();
    let mut stack = alloc::vec![String::from(target_event_id)];
    while let Some(ev_id) = stack.pop() {
        if visited.insert(ev_id.clone()) {
            if let Some(ev) = events_map.get(&ev_id) {
                for pe in &ev.prev_events {
                    stack.push(pe.clone());
                }
            }
        }
    }

    // 2. Topological sort of the reachable ancestor subgraph
    let sorted_ancestors = topological_sort_ancestors(&visited, events_map);

    // 3. Iteratively compute state after each ancestor event
    let mut state_after_map: HashMap<String, BTreeMap<(String, Option<String>), String>> =
        HashMap::new();

    for id in sorted_ancestors {
        let ev = events_map.get(&id).unwrap();

        // Compute state *before* the event by resolving the state after its immediate prev_events
        let mut state_before = BTreeMap::new();
        let mut prev_states = Vec::new();
        for pe in &ev.prev_events {
            if let Some(pe_state) = state_after_map.get(pe) {
                prev_states.push(pe_state.clone());
            }
        }

        if prev_states.len() == 1 {
            state_before = prev_states[0].clone();
        } else if prev_states.len() > 1 {
            state_before = resolve_multiple_prev_states(&prev_states, events_map);
        }

        // State *after* the event is state *before* plus the event itself if it is a state event
        let mut state_after = state_before;
        if ev.state_key.is_some() {
            state_after.insert(
                (ev.event_type.clone(), ev.state_key.clone()),
                ev.event_id.clone(),
            );
        }

        state_after_map.insert(id, state_after);
    }

    state_after_map.remove(target_event_id)
}

fn topological_sort_ancestors<S: core::hash::BuildHasher>(
    visited: &BTreeSet<String>,
    events_map: &HashMap<String, LeanEvent, S>,
) -> Vec<String> {
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();

    for id in visited {
        in_degree.insert(id.clone(), 0);
    }

    for id in visited {
        if let Some(ev) = events_map.get(id) {
            for parent in &ev.prev_events {
                if visited.contains(parent) {
                    *in_degree.entry(id.clone()).or_insert(0) += 1;
                    adjacency
                        .entry(parent.clone())
                        .or_default()
                        .push(id.clone());
                }
            }
        }
    }

    let mut queue = alloc::collections::VecDeque::new();
    for (id, &deg) in &in_degree {
        if deg == 0 {
            queue.push_back(id.clone());
        }
    }

    let mut sorted_ancestors = Vec::new();
    while let Some(id) = queue.pop_front() {
        sorted_ancestors.push(id.clone());
        if let Some(children) = adjacency.get(&id) {
            for child in children {
                if let Some(deg) = in_degree.get_mut(child) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(child.clone());
                    }
                }
            }
        }
    }

    sorted_ancestors
}

fn resolve_multiple_prev_states<S: core::hash::BuildHasher>(
    prev_states: &[BTreeMap<(String, Option<String>), String>],
    events_map: &HashMap<String, LeanEvent, S>,
) -> BTreeMap<(String, Option<String>), String> {
    let mut occurrences: HashMap<(String, Option<String>), HashMap<String, usize>> = HashMap::new();
    let num_sets = prev_states.len();
    for map in prev_states {
        for (key, val) in map {
            *occurrences
                .entry(key.clone())
                .or_default()
                .entry(val.clone())
                .or_insert(0) += 1;
        }
    }

    let mut unconflicted_state = BTreeMap::new();
    let mut conflicted_state_set = std::collections::HashSet::new();

    for (key, ids) in occurrences {
        if ids.len() == 1 && ids.values().next().unwrap() == &num_sets {
            let id_val = ids.keys().next().unwrap();
            unconflicted_state.insert(key, id_val.clone());
        } else {
            for id_val in ids.keys() {
                conflicted_state_set.insert(id_val.clone());
            }
        }
    }

    let mut conflicted_events = HashMap::new();
    for id_val in &conflicted_state_set {
        if let Some(event) = events_map.get(id_val) {
            conflicted_events.insert(id_val.clone(), event.clone());
        }
    }

    let mut auth_chain_ids = std::collections::HashSet::new();
    let mut b_stack: Vec<String> = conflicted_state_set.into_iter().collect();
    while let Some(node) = b_stack.pop() {
        if auth_chain_ids.insert(node.clone()) {
            if let Some(event) = events_map.get(&node) {
                for auth_id in &event.auth_events {
                    b_stack.push(auth_id.clone());
                }
            }
        }
    }

    for id_val in auth_chain_ids {
        if let Some(event) = events_map.get(&id_val) {
            conflicted_events.insert(id_val, event.clone());
        }
    }

    crate::resolve::resolve_lean(
        unconflicted_state,
        conflicted_events,
        events_map,
        StateResVersion::V2,
    )
}
