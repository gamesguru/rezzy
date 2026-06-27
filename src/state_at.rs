use crate::types::{LeanEvent, StateResVersion};
use crate::HashMap;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
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
#[must_use]
pub fn compute_state_at<S: core::hash::BuildHasher>(
    target_event_id: &str,
    events_map: &HashMap<String, LeanEvent, S>,
) -> Option<BTreeMap<(String, Option<String>), String>> {
    if !events_map.contains_key(target_event_id) {
        return None;
    }

    let mut memo = HashMap::new();
    compute_state_at_recursive(target_event_id, events_map, &mut memo)
}

fn compute_state_at_recursive<S: core::hash::BuildHasher>(
    target_id: &str,
    events_map: &HashMap<String, LeanEvent, S>,
    memo: &mut HashMap<String, BTreeMap<(String, Option<String>), String>>,
) -> Option<BTreeMap<(String, Option<String>), String>> {
    if let Some(cached) = memo.get(target_id) {
        return Some(cached.clone());
    }

    let ev = events_map.get(target_id)?;

    // 1. Compute state *before* the event
    let mut state_before = BTreeMap::new();
    if !ev.prev_events.is_empty() {
        let mut prev_states = Vec::new();
        for pe in &ev.prev_events {
            if let Some(pe_state) = compute_state_at_recursive(pe, events_map, memo) {
                prev_states.push(pe_state);
            }
        }

        if prev_states.len() == 1 {
            state_before = prev_states[0].clone();
        } else if prev_states.len() > 1 {
            // Multiple prev_events -> Resolve state!
            let mut occurrences: HashMap<(String, Option<String>), HashMap<String, usize>> = HashMap::new();
            let num_sets = prev_states.len();
            for map in &prev_states {
                for (key, id) in map {
                    *occurrences
                        .entry(key.clone())
                        .or_default()
                        .entry(id.clone())
                        .or_insert(0) += 1;
                }
            }

            let mut unconflicted_state = BTreeMap::new();
            let mut conflicted_state_set = std::collections::HashSet::new();

            for (key, ids) in occurrences {
                if ids.len() == 1 && ids.values().next().unwrap() == &num_sets {
                    let id = ids.keys().next().unwrap();
                    unconflicted_state.insert(key, id.clone());
                } else {
                    for id in ids.keys() {
                        conflicted_state_set.insert(id.clone());
                    }
                }
            }

            let mut conflicted_events = HashMap::new();
            for id in &conflicted_state_set {
                if let Some(event) = events_map.get(id) {
                    conflicted_events.insert(id.clone(), event.clone());
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

            for id in auth_chain_ids {
                if let Some(event) = events_map.get(&id) {
                    conflicted_events.insert(id, event.clone());
                }
            }

            state_before = crate::resolve::resolve_lean(
                unconflicted_state,
                conflicted_events,
                events_map,
                StateResVersion::V2,
            );
        }
    }

    // 2. Compute state *after* the event
    let mut state_after = state_before;
    if ev.state_key.is_some() {
        state_after.insert(
            (ev.event_type.clone(), ev.state_key.clone()),
            ev.event_id.clone(),
        );
    }

    memo.insert(target_id.to_string(), state_after.clone());
    Some(state_after)
}
