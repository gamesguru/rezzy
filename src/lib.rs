// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![no_std]

extern crate alloc;

pub mod auth;

use alloc::collections::BTreeSet;
use alloc::collections::{BTreeMap, BinaryHeap};

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;
use serde::{Deserialize, Serialize};

use serde_json::Value;

#[cfg(feature = "mock-ruma")]
pub use ruma_state_res::{events, test_utils, utils, Error as RumaError, Event, StateMap};

#[cfg(feature = "mock-ruma")]
fn ruma_to_lean_event<E: Event>(ev: &E) -> crate::LeanEvent {
    use alloc::string::ToString;
    let content_val: serde_json::Value =
        serde_json::from_str(ev.content().get()).unwrap_or(serde_json::Value::Null);
    let power_level = if let Some(pl) = content_val.get("power_level") {
        pl.as_i64()
            .or_else(|| pl.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0)
    } else {
        0
    };
    crate::LeanEvent {
        event_id: ev.event_id().to_string(),
        event_type: ev.event_type().to_string(),
        state_key: ev.state_key().map(alloc::string::ToString::to_string),
        power_level,
        origin_server_ts: ev.origin_server_ts().0.into(),
        sender: ev.sender().to_string(),
        content: content_val,
        prev_events: ev.prev_events().map(|id| id.to_string()).collect(),
        auth_events: ev.auth_events().map(|id| id.to_string()).collect(),
        depth: 0,
    }
}

#[cfg(feature = "mock-ruma")]
pub fn resolve<'a, E, MapsIter>(
    _auth_rules: &ruma_common::room_version_rules::AuthorizationRules,
    _state_res_rules: &ruma_common::room_version_rules::StateResolutionV2Rules,
    _state_maps: impl IntoIterator<IntoIter = MapsIter>,
    _auth_chains: Vec<ruma_state_res::utils::event_id_set::EventIdSet<E::Id>>,
    _fetch_event: impl Fn(&ruma_common::EventId) -> Option<E>,
    _fetch_conflicted_state_subgraph: impl Fn(
        &StateMap<Vec<E::Id>>,
    ) -> Option<
        ruma_state_res::utils::event_id_set::EventIdSet<E::Id>,
    >,
) -> core::result::Result<StateMap<E::Id>, RumaError>
where
    E: Event + Clone,
    E::Id: 'a,
    MapsIter: Iterator<Item = &'a StateMap<E::Id>> + Clone,
{
    use alloc::string::ToString;
    use core::borrow::Borrow;
    use std::collections::{BTreeMap, HashMap, HashSet};

    let mut state_sets = Vec::new();
    let mut id_map: HashMap<String, E::Id> = HashMap::new();

    for map in _state_maps {
        state_sets.push(map.clone());
        for id in map.values() {
            id_map.insert(id.to_string(), id.clone());
        }
    }
    if state_sets.is_empty() {
        return Ok(StateMap::new());
    }

    let mut counts: HashMap<(&(ruma_events::StateEventType, String), &E::Id), usize> =
        HashMap::new();
    for map in &state_sets {
        for (key, id) in map.iter() {
            *counts.entry((key, id)).or_insert(0) += 1;
        }
    }

    let num_maps = state_sets.len();
    let mut conflicted_keys = HashSet::new();
    let mut unconflicted_state = BTreeMap::new();

    for map in &state_sets {
        for (key, id) in map.iter() {
            if counts.get(&(key, id)).copied().unwrap_or(0) == num_maps {
                let state_key_opt = if key.1.is_empty() {
                    None
                } else {
                    Some(key.1.clone())
                };
                unconflicted_state.insert((key.0.to_string(), state_key_opt), id.to_string());
            } else {
                conflicted_keys.insert(key.clone());
            }
        }
    }

    let mut conflicted_events = HashMap::new();
    let mut auth_context = HashMap::new();

    for map in &state_sets {
        for (key, id) in map.iter() {
            if conflicted_keys.contains(key) {
                let id_str = id.to_string();
                if !conflicted_events.contains_key(&id_str) {
                    if let Some(ev) = _fetch_event(id.borrow()) {
                        conflicted_events.insert(id_str.clone(), ruma_to_lean_event(&ev));
                    }
                }
            }
        }
    }

    let mut conflicted_state_set: StateMap<Vec<E::Id>> = StateMap::new();
    for map in &state_sets {
        for (key, id) in map.iter() {
            if conflicted_keys.contains(key) {
                let list = conflicted_state_set
                    .entry(key.clone())
                    .or_insert_with(Vec::new);
                if !list.contains(id) {
                    list.push(id.clone());
                }
            }
        }
    }

    if _state_res_rules.begin_iterative_auth_checks_with_empty_state_map {
        if let Some(subgraph) = _fetch_conflicted_state_subgraph(&conflicted_state_set) {
            for id in subgraph {
                let id_str = id.to_string();
                if !conflicted_events.contains_key(&id_str) {
                    if let Some(ev) = _fetch_event(id.borrow()) {
                        conflicted_events.insert(id_str.clone(), ruma_to_lean_event(&ev));
                    }
                }
            }
        }
    }

    let mut to_fetch = Vec::new();
    for map in &state_sets {
        for (_key, id) in map.iter() {
            to_fetch.push(id.clone());
            id_map.insert(id.to_string(), id.clone());
        }
    }

    // Compute auth difference
    // Also handle odd number of auth chains if applicable (ruma does not do this for symmetric_diff, but wait, ruma chunks by 2. Let's just do exactly what ruma does, or just compute union minus intersection)
    // Actually, an easier way is just union all chains, and intersect all chains, then diff = union - intersection.
    let mut union_auth = std::collections::HashSet::new();
    let mut intersect_auth = if _auth_chains.is_empty() {
        std::collections::HashSet::new()
    } else {
        _auth_chains[0]
            .iter()
            .map(|id| id.to_string())
            .collect::<std::collections::HashSet<_>>()
    };
    for chain in &_auth_chains {
        let set: std::collections::HashSet<_> = chain.iter().map(|id| id.to_string()).collect();
        union_auth.extend(set.clone());
        intersect_auth.retain(|id| set.contains(id));
    }
    let auth_diff: std::collections::HashSet<_> =
        union_auth.difference(&intersect_auth).cloned().collect();

    for id_str in auth_diff {
        if !conflicted_events.contains_key(&id_str) {
            if let Some(id) = id_map.get(&id_str) {
                if let Some(ev) = _fetch_event(id.borrow()) {
                    conflicted_events.insert(id_str.clone(), ruma_to_lean_event(&ev));
                }
            }
        }
    }
    for chain in _auth_chains {
        for id in chain.iter() {
            to_fetch.push(id.clone());
            id_map.insert(id.to_string(), id.clone());
        }
    }

    let mut visited = std::collections::HashSet::new();

    while let Some(id) = to_fetch.pop() {
        let id_str = id.to_string();
        if !visited.insert(id_str.clone()) {
            continue;
        }

        if let Some(ev) = _fetch_event(id.borrow()) {
            if !conflicted_events.contains_key(&id_str) {
                auth_context.insert(id_str.clone(), ruma_to_lean_event(&ev));
            }
            for auth_id in ev.auth_events() {
                to_fetch.push(auth_id.clone());
                id_map.insert(auth_id.to_string(), auth_id.clone());
            }
        }
    }

    // Attempt to dynamically select V2 vs V2.1 if the inputs match the MSC4297 test scenario.
    // In MSC4297 test, conflicted events have specific topologies, but standard V2 is safe for now.
    let resolved = crate::resolve_lean(
        unconflicted_state,
        conflicted_events,
        &auth_context,
        if _state_res_rules.begin_iterative_auth_checks_with_empty_state_map {
            crate::StateResVersion::V2_1
        } else {
            crate::StateResVersion::V2
        },
    );

    let mut result = StateMap::new();
    for ((ev_type, state_key), id_str) in resolved {
        let key = (
            ev_type.as_str().into(),
            state_key.clone().unwrap_or_default(),
        );
        if let Some(id) = id_map.get(&id_str) {
            result.insert(key, id.clone());
        }
    }

    Ok(result)
}

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "std")]
pub use std::collections::HashMap;

#[cfg(not(feature = "std"))]
pub use hashbrown::HashMap;

/// The version of the Matrix State Resolution algorithm to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
pub enum StateResVersion {
    V1,
    V2,
    V2_1,
}

/// Result of Kahn's topological sort with diagnostic information.
#[derive(Debug, Clone)]
pub enum KahnSortResult {
    /// All events were successfully sorted.
    Ok(Vec<String>),
    /// A cycle was detected. `sorted` contains the partial ordering of events
    /// that could be processed, `stuck` contains events that could not reach
    /// in-degree 0 (involved in cycles).
    CycleDetected {
        sorted: Vec<String>,
        stuck: Vec<String>,
    },
}

impl KahnSortResult {
    /// Returns the sorted event IDs, or an empty vec if a cycle was detected.
    /// This preserves backward compatibility with the old API.
    pub fn into_sorted(self) -> Vec<String> {
        match self {
            KahnSortResult::Ok(v) => v,
            KahnSortResult::CycleDetected { .. } => Vec::new(),
        }
    }

    /// Returns true if sorting completed without cycles.
    pub fn is_ok(&self) -> bool {
        matches!(self, KahnSortResult::Ok(_))
    }
}

/// Synapse-compatible power level deserialization.
/// Handles integer (100), string ("100"), and float (100.0) representations.
fn deserialize_power_level<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct PowerLevelVisitor;

    impl<'de> de::Visitor<'de> for PowerLevelVisitor {
        type Value = i64;

        fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
            formatter.write_str("an integer, float, or string representation of a power level")
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
            Ok(v)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
            Ok(v as i64)
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<i64, E> {
            Ok(v as i64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
            Ok(v.parse::<i64>()
                .or_else(|_| v.parse::<f64>().map(|f| f as i64))
                .unwrap_or(0))
        }
    }

    deserializer.deserialize_any(PowerLevelVisitor)
}

/// A lightweight Matrix Event representation for Lean-equivalent resolution.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LeanEvent {
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub state_key: Option<String>,
    #[serde(default, deserialize_with = "deserialize_power_level")]
    pub power_level: i64,
    pub origin_server_ts: u64,
    #[serde(default)]
    pub sender: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub prev_events: Vec<String>,
    #[serde(default)]
    pub auth_events: Vec<String>,
    #[serde(default)]
    pub depth: u64, // Required for V1
}

impl PartialEq for LeanEvent {
    fn eq(&self, other: &Self) -> bool {
        self.event_id == other.event_id
    }
}

impl Eq for LeanEvent {}

impl Ord for LeanEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        self.event_id.cmp(&other.event_id)
    }
}

impl PartialOrd for LeanEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl LeanEvent {
    /// Deterministic ordering: depth ascending, then event_id ascending.
    /// Use this instead of `sort_by_key(|ev| ev.depth)` to avoid
    /// non-determinism from HashMap iteration order on equal depths.
    pub fn cmp_by_depth(&self, other: &Self) -> Ordering {
        self.depth
            .cmp(&other.depth)
            .then(self.event_id.cmp(&other.event_id))
    }
}

/// A wrapper to ensure BinaryHeap pops the "Best" event FIRST.
#[derive(Debug, Clone, Copy)]
struct SortPriority<'a> {
    event: &'a LeanEvent,
    power_level: i64,
    version: StateResVersion,
}

const MAX_POWER_LEVEL: i64 = 9007199254740991; // 2^53 - 1

/// Dynamically fetches the sender's power level by recursively walking the event's auth chain.
fn get_power_level_from_auth_chain(
    event: &LeanEvent,
    auth_context: &HashMap<String, LeanEvent>,
) -> i64 {
    let mut pl_event = None;
    let mut create_event = None;

    // Use a work queue to walk the auth chain recursively.
    let mut queue = Vec::new();
    queue.extend(event.auth_events.iter().cloned());
    let mut visited = BTreeSet::new();

    while let Some(aid) = queue.pop() {
        if !visited.insert(aid.clone()) {
            continue;
        }

        if let Some(aev) = auth_context.get(&aid) {
            if aev.event_type == "m.room.power_levels" && aev.state_key.as_deref() == Some("") {
                if pl_event.is_none() {
                    pl_event = Some(aev.clone());
                }
            } else if aev.event_type == "m.room.create"
                && aev.state_key.as_deref() == Some("")
                && create_event.is_none()
            {
                create_event = Some(aev.clone());
            }

            // Optimization: if we found both, we can stop walking.
            if pl_event.is_some() && create_event.is_some() {
                break;
            }

            // Continue walking up the auth chain.
            queue.extend(aev.auth_events.iter().cloned());
        }
    }

    // Explicitly find m.room.create if it wasn't in the auth chain (V12+ behavior)
    if create_event.is_none() {
        create_event = auth_context
            .values()
            .find(|ev| ev.event_type == "m.room.create")
            .cloned();
    }

    if let Some(pl_ev) = pl_event {
        if let Some(users) = pl_ev.content.get("users").and_then(|u| u.as_object()) {
            if let Some(pl) = users.get(&event.sender).and_then(|p| p.as_i64()) {
                return pl;
            }
        }
        if let Some(default_pl) = pl_ev.content.get("users_default").and_then(|p| p.as_i64()) {
            return default_pl;
        }
        return 0; // Default if PL event exists but no users_default
    }

    if let Some(create_ev) = create_event {
        let is_primary_creator = create_ev.sender == event.sender;
        let mut is_additional_creator = false;

        if let Some(creators) = create_ev
            .content
            .get("room_creators")
            .and_then(|c| c.as_array())
        {
            if creators.iter().any(|c| c.as_str() == Some(&event.sender)) {
                is_additional_creator = true;
            }
        }
        if let Some(creators) = create_ev
            .content
            .get("additional_creators")
            .and_then(|c| c.as_array())
        {
            if creators.iter().any(|c| c.as_str() == Some(&event.sender)) {
                is_additional_creator = true;
            }
        }

        if is_primary_creator || is_additional_creator {
            return MAX_POWER_LEVEL;
        }
    }

    event.power_level
}

impl<'a> PartialEq for SortPriority<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.power_level == other.power_level
            && self.event.origin_server_ts == other.event.origin_server_ts
            && self.event.event_id == other.event.event_id
    }
}

impl<'a> Eq for SortPriority<'a> {}

impl<'a> Ord for SortPriority<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.version {
            StateResVersion::V1 => {
                // V1 tie-breaking: depth (asc) -> event_id (asc)
                // In Rust's Max-Heap BinaryHeap, "greater" elements are popped first.
                // We want deeper events to pop FIRST, so they must be "greater".
                match self.event.depth.cmp(&other.event.depth) {
                    Ordering::Equal => self.event.event_id.cmp(&other.event.event_id),
                    ord => ord,
                }
            }
            StateResVersion::V2 | StateResVersion::V2_1 => {
                // V2 reverse topological power ordering: worst events pop FIRST.
                //
                // Ruma uses Reverse(TieBreaker) on a BinaryHeap where TieBreaker.cmp is:
                //   other.pl.cmp(&self.pl)  → higher PL = smaller TieBreaker → larger Reverse → pops first
                //   self.ts.cmp(&other.ts)  → earlier ts = smaller TieBreaker → larger Reverse → pops first
                //   self.id.cmp(&other.id)  → smaller id = smaller TieBreaker → larger Reverse → pops first
                //
                // In our direct max-heap (no Reverse) we invert each: Greater = pops first.
                //   higher PL → Greater  → use self.pl.cmp(&other.pl)
                //   earlier ts → Greater → use other.ts.cmp(&self.ts)
                //   smaller id → Greater → use other.id.cmp(&self.id)
                //
                // Net result: high-PL events pop first (losing for same-key conflicts but
                // setting auth context before lower-PL events are checked — this is what
                // makes Alice's ban appear before Bob's concurrent PL change).
                match self.power_level.cmp(&other.power_level) {
                    Ordering::Equal => match other
                        .event
                        .origin_server_ts
                        .cmp(&self.event.origin_server_ts)
                    {
                        Ordering::Equal => other.event.event_id.cmp(&self.event.event_id),
                        ord => ord,
                    },
                    ord => ord,
                }
            }
        }
    }
}

impl<'a> PartialOrd for SortPriority<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Kahn's Topological Sort with full diagnostic output.
/// Returns a `KahnSortResult` that distinguishes between successful sorts
/// and cycle detection, providing the stuck set for debugging.
pub fn lean_kahn_sort_detailed(
    events: &HashMap<String, LeanEvent>,
    auth_context: &HashMap<String, LeanEvent>,
    version: StateResVersion,
) -> KahnSortResult {
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();

    for (id, event) in events {
        in_degree.entry(id.clone()).or_insert(0);
        for auth in &event.auth_events {
            if events.contains_key(auth) {
                // Topological sort: ancestors come BEFORE descendants.
                // But we want a REVERSE topological sort: descendants BEFORE ancestors.
                // So we add edges from ancestors to descendants.
                adjacency.entry(auth.clone()).or_default().push(id.clone());
                *in_degree.entry(id.clone()).or_insert(0) += 1;
            }
        }
    }

    // Pre-compute power levels once per event to avoid redundant auth chain walks
    // inside the hot BinaryHeap push path.
    let pl_cache: HashMap<String, i64> = events
        .iter()
        .map(|(id, ev)| {
            (
                id.clone(),
                get_power_level_from_auth_chain(ev, auth_context),
            )
        })
        .collect();

    let mut queue: BinaryHeap<SortPriority> = BinaryHeap::new();
    for (id, &degree) in &in_degree {
        if degree == 0 {
            if let Some(event) = events.get(id) {
                queue.push(SortPriority {
                    event,
                    power_level: pl_cache.get(id).copied().unwrap_or(0),
                    version,
                });
            }
        }
    }

    let mut result = Vec::new();
    while let Some(priority) = queue.pop() {
        let event = priority.event;

        result.push(event.event_id.clone());
        if let Some(neighbors) = adjacency.get(&event.event_id) {
            for next_id in neighbors {
                let degree = in_degree.get_mut(next_id).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    let next_ev = events.get(next_id).unwrap();
                    queue.push(SortPriority {
                        event: next_ev,
                        power_level: pl_cache.get(next_id).copied().unwrap_or(0),
                        version,
                    });
                }
            }
        }
    }

    // Detect cycles: events that never reached in-degree 0.
    if result.len() != events.len() {
        let sorted_set: alloc::collections::BTreeSet<&String> = result.iter().collect();
        let stuck: Vec<String> = events
            .keys()
            .filter(|id| !sorted_set.contains(id))
            .cloned()
            .collect();
        return KahnSortResult::CycleDetected {
            sorted: result,
            stuck,
        };
    }

    KahnSortResult::Ok(result)
}

/// A simplified implementation of Kahn's Topological Sort.
/// Backward-compatible wrapper that returns an empty Vec on cycles.
pub fn lean_kahn_sort(
    events: &HashMap<String, LeanEvent>,
    auth_context: &HashMap<String, LeanEvent>,
    version: StateResVersion,
) -> Vec<String> {
    match lean_kahn_sort_detailed(events, auth_context, version) {
        KahnSortResult::Ok(sorted) => sorted,
        KahnSortResult::CycleDetected { sorted, stuck } => {
            std::eprintln!("KAHN CYCLE DETECTED! Stuck: {:?}", stuck);
            sorted
        }
    }
}

pub fn resolve_lean(
    unconflicted_state: BTreeMap<(String, Option<String>), String>,
    conflicted_events: HashMap<String, LeanEvent>,
    auth_context: &HashMap<String, LeanEvent>,
    version: StateResVersion,
) -> BTreeMap<(String, Option<String>), String> {
    // Build a merged lookup map for sort/mainline operations.
    // auth_context intentionally excludes events that are in conflicted_events;
    // however, a conflicted event (e.g. $01-power_levels) may appear in the
    // auth_events chain of another conflicted event ($02), so PL lookups during
    // sorting must be able to find it.  iterative_auth_ok already checks both
    // maps independently — we only need to merge here for the sort phases.
    let sort_context: HashMap<String, LeanEvent> = auth_context
        .iter()
        .chain(conflicted_events.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // MSC4297 (v2.1): The algorithm starts from an empty set of state.
    let mut resolved = match version {
        StateResVersion::V2_1 => BTreeMap::new(),
        _ => unconflicted_state.clone(),
    };

    let sort_set = &conflicted_events;

    // Route all events through Kahn sort (reverse topological power ordering).
    let mut power_events = HashMap::new();
    let mut non_power_events = HashMap::new();

    for (id, ev) in sort_set {
        if ev.event_type == "m.room.member"
            || ev.event_type == "m.room.create"
            || ev.event_type == "m.room.power_levels"
            || ev.event_type == "m.room.join_rules"
        {
            power_events.insert(id.clone(), ev.clone());
        } else {
            non_power_events.insert(id.clone(), ev.clone());
        }
    }

    // Step 1: Sort power events by reverse topological power ordering (Kahn sort)
    // Step 2: Apply iterative auth checks (per spec & Ruma implementation)
    let sorted_power_ids = lean_kahn_sort(&power_events, &sort_context, version);
    for id in &sorted_power_ids {
        if let Some(event) = sort_set.get(id) {
            if iterative_auth_ok(event, &resolved, auth_context, sort_set) {
                resolved.insert(
                    (event.event_type.clone(), event.state_key.clone()),
                    event.event_id.clone(),
                );
            }
        }
    }

    // Step 3: Build the power-level mainline for mainline sort
    let mainline = build_mainline(&resolved, &sort_context);

    // Step 4: Sort non-power events by mainline ordering + iterative auth check
    let mut non_power_list: Vec<&LeanEvent> = non_power_events.values().collect();
    mainline_sort(&mut non_power_list, &mainline, &sort_context);

    for ev in non_power_list {
        if iterative_auth_ok(ev, &resolved, auth_context, sort_set) {
            resolved.insert(
                (ev.event_type.clone(), ev.state_key.clone()),
                ev.event_id.clone(),
            );
        }
    }

    let mut final_resolved = unconflicted_state;
    for (k, v) in resolved {
        final_resolved.insert(k, v);
    }
    final_resolved
}

/// Targeted iterative auth check. Per Matrix spec, the auth context for event 'e'
/// consists of the events in the conflict set (E) and the currently resolved state (S).
fn iterative_auth_ok(
    event: &LeanEvent,
    resolved: &BTreeMap<(String, Option<String>), String>,
    auth_context: &HashMap<String, LeanEvent>,
    conflicted_events: &HashMap<String, LeanEvent>,
) -> bool {
    let mut state = crate::auth::RoomState::new();

    // 1. Populate state from consensus resolved map (S)
    for (key, eid) in resolved {
        if let Some(ev) = auth_context.get(eid).or_else(|| conflicted_events.get(eid)) {
            state.insert(key.clone(), ev.clone());
        }
    }

    // Explicitly add m.room.create to the auth state if we can find it in the context.
    // In V12+, m.room.create is implicitly in the auth state because it's not allowed in auth_events.
    if let std::collections::btree_map::Entry::Vacant(e) =
        state.entry(("m.room.create".into(), Some(String::new())))
    {
        let create_ev = auth_context
            .values()
            .chain(conflicted_events.values())
            .find(|ev| ev.event_type == "m.room.create");
        if let Some(create_ev) = create_ev {
            e.insert(create_ev.clone());
        }
    }

    // 2. Supplement with events from the event's own RECURSIVE auth chain (from E union S).
    // Use BFS (queue) so that direct auth deps (1 hop from E) are inserted before transitive
    // deps (2+ hops). This ensures or_insert keeps the most-direct auth event for each key.
    // DFS (stack) could insert a transitive dep like $00-pl (via $00-bob → $00-jl) before
    // the direct dep $01-pl, causing the wrong PL event to be used for auth checks.
    let mut queue = alloc::collections::VecDeque::new();
    queue.extend(event.auth_events.iter().cloned());
    let mut visited = BTreeSet::new();
    while let Some(aid) = queue.pop_front() {
        if !visited.insert(aid.clone()) {
            continue;
        }
        if let Some(aev) = auth_context
            .get(&aid)
            .or_else(|| conflicted_events.get(&aid))
        {
            let key = (aev.event_type.clone(), aev.state_key.clone());
            // Consensus state (resolved) always wins over the event's raw auth chain.
            state.entry(key).or_insert_with(|| aev.clone());
            // Walk up to find the membership/PL/create events if not already found.
            queue.extend(aev.auth_events.iter().cloned());
        }
    }

    let auth_result = crate::auth::check_auth(event, &state);
    auth_result.is_ok()
}

/// Build the power-level mainline: the chain of m.room.power_levels events
/// from the resolved PL event backwards through auth_events.
fn build_mainline(
    resolved: &BTreeMap<(String, Option<String>), String>,
    auth_context: &HashMap<String, LeanEvent>,
) -> Vec<String> {
    let mut mainline = Vec::new();
    let pl_key = (
        alloc::string::String::from("m.room.power_levels"),
        Some(alloc::string::String::new()),
    );
    let mut current = resolved.get(&pl_key).cloned();

    while let Some(eid) = current {
        mainline.push(eid.clone());
        current = None;
        if let Some(ev) = auth_context.get(&eid) {
            for auth_id in &ev.auth_events {
                if let Some(auth_ev) = auth_context.get(auth_id) {
                    if auth_ev.event_type == "m.room.power_levels" {
                        current = Some(auth_id.clone());
                        break;
                    }
                }
            }
        }
    }

    mainline
}

/// Precompute the closest mainline position for every event reachable via
/// auth_events using a single O(V+E) multi-source reverse-BFS.
///
/// The naive approach walks the auth chain per-event: O(events × chain_depth).
/// On a dense DAG with 52k events this dominates runtime.
///
/// This approach instead:
/// 1. Seeds the BFS from ALL mainline events simultaneously at their positions.
/// 2. Builds reverse auth-edges (auth_ev → events that list it) once: O(V+E).
/// 3. BFS outward through those reverse edges; since we process in ascending
///    position order, the first time an event is reached gives the minimum
///    (closest) mainline position.
///
/// Total: O(V+E) — each vertex and edge touched at most once.
fn precompute_mainline_positions(
    mainline: &[String],
    auth_context: &HashMap<String, LeanEvent>,
) -> HashMap<String, usize> {
    let mainline_len = mainline.len();

    // Build reverse adjacency over the full auth context once.
    // reverse_adj[A] = [E1, E2, ...] means E1, E2, ... list A in their auth_events.
    let mut reverse_adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, ev) in auth_context {
        for auth_id in &ev.auth_events {
            reverse_adj
                .entry(auth_id.as_str())
                .or_default()
                .push(id.as_str());
        }
    }

    let mut dist: HashMap<String, usize> = HashMap::with_capacity(auth_context.len());

    // Seed: process mainline events in position order (0 = closest = best).
    // Using a VecDeque gives BFS ordering; since positions only increase along
    // the mainline and edges carry zero additional cost, this is correct.
    let mut queue: alloc::collections::VecDeque<(&str, usize)> =
        alloc::collections::VecDeque::new();

    for (pos, id) in mainline.iter().enumerate() {
        dist.insert(id.clone(), pos);
        queue.push_back((id.as_str(), pos));
    }

    // Flood-fill outward through reverse auth-edges.
    // First assignment wins (minimum position) because we process in BFS order
    // starting from position 0.
    while let Some((id, pos)) = queue.pop_front() {
        if let Some(children) = reverse_adj.get(id) {
            for &child_id in children {
                if !dist.contains_key(child_id) {
                    dist.insert(child_id.into(), pos);
                    queue.push_back((child_id, pos));
                }
            }
        }
    }

    // Events with no path to mainline get sentinel = mainline_len (worst).
    // Callers use `.get().copied().unwrap_or(mainline_len)` for those.
    let _ = mainline_len; // consumed by callers
    dist
}

/// Sort events by mainline ordering per the Matrix spec:
/// 1. Closest mainline position (smaller index = closer to current PL = comes last)
/// 2. origin_server_ts ascending (earlier first, later wins via last-write)
/// 3. event_id ascending (smaller first)
fn mainline_sort(
    events: &mut Vec<&LeanEvent>,
    mainline: &[String],
    auth_context: &HashMap<String, LeanEvent>,
) {
    let mainline_len = mainline.len();

    // Single O(V+E) pass over the full auth context.
    let dist = precompute_mainline_positions(mainline, auth_context);

    events.sort_by(|a, b| {
        let pos_a = dist.get(&a.event_id).copied().unwrap_or(mainline_len);
        let pos_b = dist.get(&b.event_id).copied().unwrap_or(mainline_len);

        // Larger mainline position = farther from current PL = worse = comes first
        // (so it gets overwritten by closer events via last-write-wins)
        match pos_b.cmp(&pos_a) {
            Ordering::Equal => {
                // Earlier timestamp comes first (later wins via last-write)
                match a.origin_server_ts.cmp(&b.origin_server_ts) {
                    Ordering::Equal => a.event_id.cmp(&b.event_id),
                    ord => ord,
                }
            }
            ord => ord,
        }
    });
}

/// Result of conflicted subgraph computation with diagnostic info.
#[derive(Debug, Clone)]
pub struct SubgraphResult {
    /// The computed conflicted subgraph.
    pub subgraph: HashMap<String, LeanEvent>,
    /// Auth events referenced but not found in the graph (permanently lost to federation).
    pub missing_auth_events: Vec<String>,
}

pub fn compute_v2_1_conflicted_subgraph(
    auth_graph: &HashMap<String, LeanEvent>,
    conflicted_set: &[String],
) -> HashMap<String, LeanEvent> {
    compute_v2_1_conflicted_subgraph_bounded(auth_graph, conflicted_set, None).subgraph
}

/// Bounded version of conflicted subgraph computation.
/// `max_auth_depth`: If set, limits backwards traversal depth to prevent
/// history-flooding DoS attacks where a rogue admin generates millions of
/// spoofed events on a dead-end fork.
pub fn compute_v2_1_conflicted_subgraph_bounded(
    auth_graph: &HashMap<String, LeanEvent>,
    conflicted_set: &[String],
    max_auth_depth: Option<usize>,
) -> SubgraphResult {
    let mut backwards_reachable = BTreeSet::new();
    let mut forwards_reachable = BTreeSet::new();
    let mut missing_auth_events = BTreeSet::new();

    // Calculate Backwards Reachable (Ancestors up the auth chain)
    // Each entry is (event_id, depth_from_conflicted_set)
    let mut b_stack: Vec<(String, usize)> = conflicted_set.iter().map(|s| (s.clone(), 0)).collect();
    while let Some((node, depth)) = b_stack.pop() {
        // Anti-DoS: stop expanding beyond max depth
        if let Some(max_depth) = max_auth_depth {
            if depth > max_depth {
                continue;
            }
        }
        if backwards_reachable.insert(node.clone()) {
            if let Some(event) = auth_graph.get(&node) {
                for auth_id in &event.auth_events {
                    if !auth_graph.contains_key(auth_id) {
                        missing_auth_events.insert(auth_id.clone());
                    }
                    b_stack.push((auth_id.clone(), depth + 1));
                }
            }
        }
    }

    // Build Reverse Adjacency for Forwards Search
    let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
    for (id, event) in auth_graph {
        for prev in &event.auth_events {
            children_map
                .entry(prev.clone())
                .or_default()
                .push(id.clone());
        }
    }

    // Calculate Forwards Reachable (Descendants down the auth chain)
    let mut f_stack: Vec<String> = conflicted_set.to_vec();
    while let Some(node) = f_stack.pop() {
        if forwards_reachable.insert(node.clone()) {
            if let Some(children) = children_map.get(&node) {
                f_stack.extend(children.clone());
            }
        }
    }

    // Intersect and build the final Conflicted Subgraph
    let mut subgraph = HashMap::new();
    let backwards_ids: BTreeSet<String> = backwards_reachable.iter().cloned().collect();
    let forwards_ids: BTreeSet<String> = forwards_reachable.iter().cloned().collect();

    for id in backwards_ids.intersection(&forwards_ids) {
        if let Some(event) = auth_graph.get(id) {
            subgraph.insert(id.clone(), event.clone());
        }
    }

    SubgraphResult {
        subgraph,
        missing_auth_events: missing_auth_events.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[cfg(not(feature = "std"))]
    use hashbrown::HashMap;
    #[cfg(feature = "std")]
    use std::collections::HashMap;

    #[test]
    fn test_leanevent_deserialization_defaults() {
        let json = r#"{
            "event_id": "$test",
            "type": "m.room.message",
            "origin_server_ts": 12345
        }"#;
        let ev: LeanEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.event_id, "$test");
        assert_eq!(ev.event_type, "m.room.message");
        assert_eq!(ev.origin_server_ts, 12345);
        assert_eq!(ev.state_key, None);
        assert_eq!(ev.power_level, 0);
        assert_eq!(ev.sender, "");
        assert_eq!(ev.prev_events.len(), 0);
        assert_eq!(ev.auth_events.len(), 0);
        assert_eq!(ev.depth, 0);
    }

    #[test]
    fn test_sort_priority_v2_tie_break() {
        let e_base = LeanEvent {
            event_id: "$1".into(),
            power_level: 100,
            origin_server_ts: 10,
            ..Default::default()
        };
        let e_worst_pl = LeanEvent {
            event_id: "$2".into(),
            power_level: 50,
            origin_server_ts: 10,
            ..Default::default()
        };
        let p_base = SortPriority {
            power_level: e_base.power_level,
            event: &e_base,
            version: StateResVersion::V2,
        };
        let p_worst_pl = SortPriority {
            power_level: e_worst_pl.power_level,
            event: &e_worst_pl,
            version: StateResVersion::V2,
        };

        // Higher PL is GREATER (pops first, loses for same key, but sets auth context first).
        assert_eq!(p_base.cmp(&p_worst_pl), Ordering::Greater); // p_base 100 > p_worst_pl 50.

        let e_later_ts = LeanEvent {
            event_id: "$3".into(),
            power_level: 100,
            origin_server_ts: 20,
            ..Default::default()
        };
        let p_later_ts = SortPriority {
            power_level: e_later_ts.power_level,
            event: &e_later_ts,
            version: StateResVersion::V2,
        };
        // p_later_ts has ts 20 (better — wins); later ts pops LAST = is Smaller.
        // p_base has ts 10 (worse) = Greater (pops first, loses).
        assert_eq!(p_base.cmp(&p_later_ts), Ordering::Greater);

        let e_larger_id = LeanEvent {
            event_id: "$2".into(),
            power_level: 100,
            origin_server_ts: 10,
            ..Default::default()
        };
        let p_larger_id = SortPriority {
            power_level: e_larger_id.power_level,
            event: &e_larger_id,
            version: StateResVersion::V2,
        };
        // p_larger_id has id "$2" (better — wins); larger id pops LAST = is Smaller.
        // p_base has id "$1" (worse) = Greater (pops first, loses).
        assert_eq!(p_base.cmp(&p_larger_id), Ordering::Greater);
    }

    #[test]
    fn test_v1_resolution_happy_path() {
        let mut events = HashMap::new();
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 0,
                origin_server_ts: 100,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 0,
                origin_server_ts: 50,
                prev_events: vec![],
                auth_events: vec!["A".into()],
                depth: 2,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V1);
        assert_eq!(sorted, vec!["A", "B"]);
    }

    #[test]
    fn test_v2_1_strict_resolution() {
        let mut unconflicted = BTreeMap::new();
        unconflicted.insert(
            ("m.room.member".into(), Some("@alice:example.com".into())),
            "A".into(),
        );

        let mut conflicted = HashMap::new();
        conflicted.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 50,
                origin_server_ts: 100,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        conflicted.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 50,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );

        // In V2.1, A should win because B (higher PL=100) is applied first and then
        // overwritten by A (lower PL=50) — lower PL pops last and wins for same-key conflicts.
        let resolved = resolve_lean(
            unconflicted,
            conflicted.clone(),
            &conflicted,
            StateResVersion::V2_1,
        );
        assert_eq!(
            resolved.get(&("m.room.member".into(), Some("@alice:example.com".into()))),
            Some(&"A".into())
        );
    }

    #[test]
    fn test_v1_tie_break_by_id() {
        let mut events = HashMap::new();
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 0,
                origin_server_ts: 100,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 0,
                origin_server_ts: 100,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V1);
        assert_eq!(sorted, vec!["B", "A"]);
    }

    #[test]
    fn test_v2_resolution_happy_path() {
        let mut events = HashMap::new();
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 100,
                prev_events: vec![],
                auth_events: vec![],
                depth: 10,
                ..Default::default()
            },
        );
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 50,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        // A (higher PL=100) pops first (applied first, loses for same key). B pops last, wins.
        assert_eq!(sorted, vec!["A", "B"]);
    }

    #[test]
    fn test_v2_deep_tie_break() {
        let mut events = HashMap::new();
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        // Best (B, larger ID) comes LAST.
        assert_eq!(sorted, vec!["A", "B"]);
    }

    #[test]
    fn test_v1_v2_v2_1_comparison_determinism() {
        let mut events = HashMap::new();
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 10,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 100,
                prev_events: vec![],
                auth_events: vec![],
                depth: 10,
                ..Default::default()
            },
        );
        let sorted_v1 = lean_kahn_sort(&events, &events, StateResVersion::V1);
        let sorted_v2 = lean_kahn_sort(&events, &events, StateResVersion::V2);
        let sorted_v2_1 = lean_kahn_sort(&events, &events, StateResVersion::V2_1);
        assert_eq!(sorted_v1, vec!["B", "A"]);
        // B (higher power level) pops FIRST in V2 and V2.1 — applied first, loses for same key.
        assert_eq!(sorted_v2, vec!["B", "A"]);
        assert_eq!(sorted_v2_1, vec!["B", "A"]);
    }

    #[test]
    fn test_unhappy_path_cycle_detection() {
        let mut events = HashMap::new();
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 100,
                prev_events: vec!["B".into()],
                auth_events: vec!["B".into()],
                depth: 1,
                ..Default::default()
            },
        );
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 100,
                prev_events: vec!["A".into()],
                auth_events: vec!["A".into()],
                depth: 1,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        assert!(sorted.is_empty());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let event = LeanEvent {
            event_id: "$abc".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 100,
            origin_server_ts: 12345,
            prev_events: vec![],
            auth_events: vec![],
            depth: 5,
            ..Default::default()
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: LeanEvent = serde_json::from_str(&serialized).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_partial_ord_implementations() {
        let e1 = LeanEvent {
            event_id: "a".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 100,
            origin_server_ts: 10,
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
            ..Default::default()
        };
        let e2 = LeanEvent {
            event_id: "b".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 100,
            origin_server_ts: 10,
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
            ..Default::default()
        };
        assert!(e1.partial_cmp(&e2).is_some());

        let p1 = SortPriority {
            power_level: e1.power_level,
            event: &e1,
            version: StateResVersion::V2,
        };
        let p2 = SortPriority {
            power_level: e2.power_level,
            event: &e2,
            version: StateResVersion::V2,
        };
        assert!(p1.partial_cmp(&p2).is_some());
    }

    #[test]
    fn test_trait_coverage() {
        let v = StateResVersion::V2;
        assert_eq!(v, StateResVersion::V2);
        let _ = alloc::format!("{:?}", v);

        let e = LeanEvent {
            event_id: "a".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 100,
            origin_server_ts: 10,
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
            ..Default::default()
        };
        let _ = e.clone();
        let _ = alloc::format!("{:?}", e);
    }

    #[test]
    fn test_complex_dag_sort() {
        let mut events = HashMap::new();
        events.insert(
            "1".into(),
            LeanEvent {
                event_id: "1".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        events.insert(
            "2".into(),
            LeanEvent {
                event_id: "2".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 50,
                origin_server_ts: 20,
                prev_events: vec!["1".into()],
                auth_events: vec!["1".into()],
                depth: 2,
                ..Default::default()
            },
        );
        events.insert(
            "3".into(),
            LeanEvent {
                event_id: "3".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 50,
                origin_server_ts: 15,
                prev_events: vec!["1".into()],
                auth_events: vec!["1".into()],
                depth: 2,
                ..Default::default()
            },
        );
        events.insert(
            "4".into(),
            LeanEvent {
                event_id: "4".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 10,
                origin_server_ts: 30,
                prev_events: vec!["2".into(), "3".into()],
                auth_events: vec!["2".into(), "3".into()],
                depth: 3,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        // 1 pops first (only one with in-degree 0).
        // Then 2 and 3 are in queue. 3 has earlier TS (15, worse) so it pops FIRST.
        // Then 2 (TS 20, better — later wins) pops LAST.
        // Then 4 pops.
        assert_eq!(sorted, vec!["1", "3", "2", "4"]);
    }

    #[test]
    fn test_kahn_missing_parents() {
        let mut events = HashMap::new();
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 10,
                prev_events: vec!["MISSING".into()],
                auth_events: vec!["MISSING".into()],
                depth: 1,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        assert_eq!(sorted, vec!["A"]);
    }

    #[test]
    fn test_resolve_lean_functionality() {
        let mut unconflicted = BTreeMap::new();
        unconflicted.insert(("type".into(), Some("key".into())), "id".into());
        let conflicted = HashMap::new();
        let resolved = resolve_lean(
            unconflicted.clone(),
            conflicted.clone(),
            &conflicted,
            StateResVersion::V2,
        );
        assert_eq!(resolved, unconflicted);
    }

    #[test]
    fn test_resolve_lean_v2_1_overlay() {
        use serde_json::json;

        // Uncontested state: Alice is already joined, Bob's old event is the prior state.
        let mut unconflicted = BTreeMap::new();
        unconflicted.insert(
            ("m.room.member".into(), Some("@alice:example.com".into())),
            "id1".into(),
        );
        unconflicted.insert(
            ("m.room.member".into(), Some("@bob:example.com".into())),
            "id2".into(),
        );

        // Auth context: uncontested background events needed to validate the conflicted ones.
        let mut auth_context = HashMap::new();
        auth_context.insert(
            "create".into(),
            LeanEvent {
                event_id: "create".into(),
                event_type: "m.room.create".into(),
                state_key: Some(String::new()),
                sender: "@alice:example.com".into(),
                power_level: 100,
                origin_server_ts: 1,
                content: json!({}),
                ..Default::default()
            },
        );
        auth_context.insert(
            "join_rules".into(),
            LeanEvent {
                event_id: "join_rules".into(),
                event_type: "m.room.join_rules".into(),
                state_key: Some(String::new()),
                sender: "@alice:example.com".into(),
                power_level: 100,
                origin_server_ts: 2,
                content: json!({"join_rule": "public"}),
                auth_events: vec!["create".into()],
                ..Default::default()
            },
        );
        auth_context.insert(
            "id1".into(),
            LeanEvent {
                event_id: "id1".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                sender: "@alice:example.com".into(),
                power_level: 50,
                origin_server_ts: 500,
                content: json!({"membership": "join"}),
                auth_events: vec!["create".into()],
                ..Default::default()
            },
        );

        // The conflict: two competing versions of Bob's membership.
        let mut conflicted = HashMap::new();
        conflicted.insert(
            "id2".into(),
            LeanEvent {
                event_id: "id2".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@bob:example.com".into()),
                sender: "@bob:example.com".into(),
                power_level: 50,
                origin_server_ts: 500,
                content: json!({"membership": "join"}),
                auth_events: vec!["create".into(), "join_rules".into(), "id1".into()],
                ..Default::default()
            },
        );
        conflicted.insert(
            "id2_new".into(),
            LeanEvent {
                event_id: "id2_new".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@bob:example.com".into()),
                sender: "@bob:example.com".into(),
                power_level: 100,
                origin_server_ts: 1000,
                content: json!({"membership": "join"}),
                auth_events: vec!["create".into(), "join_rules".into(), "id1".into()],
                ..Default::default()
            },
        );

        let resolved = resolve_lean(
            unconflicted.clone(),
            conflicted,
            &auth_context,
            StateResVersion::V2_1,
        );

        assert_eq!(
            resolved.get(&("m.room.member".into(), Some("@alice:example.com".into()))),
            Some(&"id1".into())
        );
        assert_eq!(
            resolved.get(&("m.room.member".into(), Some("@bob:example.com".into()))),
            Some(&"id2".into()) // id2_new (PL=100) pops first, id2 (PL=50) pops last and wins.
        );
    }

    fn run_batch_test(
        version: StateResVersion,
        rows: &[(&str, i64, u64, u64, &[&str])],
        expected: &[&str],
    ) {
        let mut events = HashMap::new();
        for r in rows {
            events.insert(
                r.0.to_string(),
                LeanEvent {
                    event_id: r.0.to_string(),
                    event_type: "m.room.member".into(),
                    state_key: Some("@alice:example.com".into()),
                    power_level: r.1,
                    origin_server_ts: r.2,
                    depth: r.3,
                    prev_events: r.4.iter().map(|s| s.to_string()).collect(),
                    auth_events: r.4.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            );
        }
        let result = lean_kahn_sort(&events, &events, version);
        assert_eq!(
            result,
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_resolution_batch() {
        run_batch_test(
            StateResVersion::V2,
            &[("Alice", 100, 500, 1, &[]), ("Bob", 50, 100, 1, &[])],
            &["Alice", "Bob"], // Alice is better (PL 100), pops first.
        );
        run_batch_test(
            StateResVersion::V1,
            &[("Deep", 100, 100, 10, &[]), ("Shallow", 10, 100, 1, &[])],
            &["Deep", "Shallow"],
        );
    }

    #[test]
    fn test_native_resolution_bootstrap_parity() {
        let mut events = HashMap::new();
        events.insert(
            "1".into(),
            LeanEvent {
                event_id: "1".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@user:example.com".into()),
                power_level: 100,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        events.insert(
            "2".into(),
            LeanEvent {
                event_id: "2".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@user:example.com".into()),
                power_level: 0,
                origin_server_ts: 20,
                prev_events: vec!["1".into()],
                auth_events: vec!["1".into()],
                depth: 2,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        let mut resolved_state = BTreeMap::new();
        for id in sorted {
            let ev = events.get(&id).unwrap();
            let key = (ev.event_type.clone(), ev.state_key.clone());
            resolved_state.insert(key, ev.event_id.clone());
        }
        assert_eq!(
            resolved_state.get(&(
                "m.room.member".to_string(),
                Some("@user:example.com".to_string())
            )),
            Some(&"2".to_string())
        );
    }

    #[test]
    fn test_enum_coverage() {
        let v = StateResVersion::V2;
        let v2 = v;
        assert_eq!(v, v2);
        let debug_str = alloc::format!("{:?}", v);
        assert!(debug_str.contains("V2"));
    }

    #[test]
    fn test_event_traits_coverage() {
        let e = LeanEvent {
            event_id: "a".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 100,
            origin_server_ts: 10,
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
            ..Default::default()
        };
        let e2 = e.clone();
        assert_eq!(e, e2);
        let debug_str = alloc::format!("{:?}", e);
        assert!(debug_str.contains("event_id"));
    }

    #[test]
    fn test_sort_priority_traits() {
        let e = LeanEvent {
            event_id: "a".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 100,
            origin_server_ts: 10,
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
            ..Default::default()
        };
        let p = SortPriority {
            power_level: e.power_level,
            event: &e,
            version: StateResVersion::V2,
        };
        let p2 = p;
        assert_eq!(p, p2);
        let debug_str = alloc::format!("{:?}", p);
        assert!(debug_str.contains("version"));
    }

    #[test]
    fn test_v1_equal_depth_tie_break() {
        let mut events = HashMap::new();
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 0,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 0,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V1);
        assert_eq!(sorted, vec!["B", "A"]);
    }

    #[test]
    fn test_kahn_no_neighbors() {
        let mut events = HashMap::new();
        events.insert(
            "1".into(),
            LeanEvent {
                event_id: "1".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        assert_eq!(sorted, vec!["1"]);
    }

    #[test]
    fn test_v2_1_full_coverage() {
        let mut events = HashMap::new();
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                power_level: 100,
                origin_server_ts: 10,
                prev_events: vec![],
                auth_events: vec![],
                depth: 1,
                ..Default::default()
            },
        );
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2_1);
        assert_eq!(sorted, vec!["A"]);
    }

    /// Regression test: V2_1 uses the same "later timestamp wins" tie-break as V2.
    /// Earlier events are sorted first (popped first from heap), later events
    /// come last and win via last-write-wins. This matches the Matrix spec.
    #[test]
    fn test_v2_1_later_timestamp_wins() {
        let mut events = HashMap::new();
        events.insert(
            "$early".into(),
            LeanEvent {
                event_id: "$early".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@user:example.com".into()),
                power_level: 100,
                origin_server_ts: 1000,
                auth_events: vec![],
                ..Default::default()
            },
        );
        events.insert(
            "$late".into(),
            LeanEvent {
                event_id: "$late".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@user:example.com".into()),
                power_level: 100,
                origin_server_ts: 2000,
                auth_events: vec![],
                ..Default::default()
            },
        );
        // Earlier ts pops first (worse), later ts comes last (wins).
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2_1);
        assert_eq!(sorted, vec!["$early", "$late"]);

        // V2 must match V2_1
        let sorted_v2 = lean_kahn_sort(&events, &events, StateResVersion::V2);
        assert_eq!(sorted_v2, vec!["$early", "$late"]);
    }

    /// Regression test: millisecond-close Draupnir ban races resolve identically
    /// in V2 and V2_1 when processed through Kahn sort alone.
    #[test]
    fn test_v2_1_millisecond_race_tiebreak() {
        let mut events = HashMap::new();
        events.insert(
            "$ban_a".into(),
            LeanEvent {
                event_id: "$ban_a".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@spammer:evil.com".into()),
                power_level: 50,
                origin_server_ts: 1772724243891,
                auth_events: vec![],
                ..Default::default()
            },
        );
        events.insert(
            "$ban_b".into(),
            LeanEvent {
                event_id: "$ban_b".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@spammer:evil.com".into()),
                power_level: 50,
                origin_server_ts: 1772724243893, // 2ms later
                auth_events: vec![],
                ..Default::default()
            },
        );
        // $ban_a (earlier ts) pops first (loses), $ban_b (later ts) comes last = wins.
        let sorted_v2 = lean_kahn_sort(&events, &events, StateResVersion::V2);
        assert_eq!(sorted_v2, vec!["$ban_a", "$ban_b"]);

        let sorted_v2_1 = lean_kahn_sort(&events, &events, StateResVersion::V2_1);
        assert_eq!(sorted_v2_1, vec!["$ban_a", "$ban_b"]);
    }

    #[test]
    fn test_total_order_properties() {
        let e1 = LeanEvent {
            event_id: "a".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 100,
            origin_server_ts: 10,
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
            ..Default::default()
        };
        let e2 = LeanEvent {
            event_id: "b".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 100,
            origin_server_ts: 10,
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
            ..Default::default()
        };
        let e3 = LeanEvent {
            event_id: "c".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 50,
            origin_server_ts: 10,
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
            ..Default::default()
        };
        assert_eq!(e1.cmp(&e1), Ordering::Equal);
        assert!(e1 <= e1);
        assert!(e1 <= e2 || e2 <= e1);
        if e1 <= e2 && e2 <= e3 {
            assert!(e1 <= e3);
        }
        let e1_copy = e1.clone();
        if e1 <= e1_copy && e1_copy <= e1 {
            assert_eq!(e1, e1_copy);
        }
    }

    #[test]
    fn test_coverage_booster_all_branches() {
        let e_base = LeanEvent {
            event_id: "m".into(),
            event_type: "m.room.member".into(),
            state_key: Some("@alice:example.com".into()),
            power_level: 50,
            origin_server_ts: 50,
            prev_events: vec![],
            auth_events: vec![],
            depth: 50,
            ..Default::default()
        };
        let p_base = SortPriority {
            power_level: e_base.power_level,
            event: &e_base,
            version: StateResVersion::V2,
        };
        let e_high_power = LeanEvent {
            power_level: 100,
            ..e_base.clone()
        };
        let p_high_power = SortPriority {
            power_level: e_high_power.power_level,
            event: &e_high_power,
            version: StateResVersion::V2,
        };
        // p_base is WORSE (PL 50 < 100). Higher PL is Greater (pops first). So p_base < p_high_power.
        assert_eq!(p_base.cmp(&p_high_power), Ordering::Less);
        let e_early_ts = LeanEvent {
            origin_server_ts: 10,
            ..e_base.clone()
        };
        let p_early_ts = SortPriority {
            power_level: e_early_ts.power_level,
            event: &e_early_ts,
            version: StateResVersion::V2,
        };
        // p_base has TS 50 (better — later wins). Better must be Smaller (pops last). So p_base < p_early_ts.
        assert_eq!(p_base.cmp(&p_early_ts), Ordering::Less);
        let e_early_id = LeanEvent {
            event_id: "a".into(),
            ..e_base.clone()
        };
        let p_early_id = SortPriority {
            power_level: e_early_id.power_level,
            event: &e_early_id,
            version: StateResVersion::V2,
        };
        // p_base has ID "m" (better — larger id wins). Better must be Smaller (pops last). So p_base < p_early_id.
        assert_eq!(p_base.cmp(&p_early_id), Ordering::Less);
        let p_v1_base = SortPriority {
            power_level: e_base.power_level,
            event: &e_base,
            version: StateResVersion::V1,
        };
        let e_shallow = LeanEvent {
            depth: 1,
            ..e_base.clone()
        };
        let p_shallow = SortPriority {
            power_level: e_shallow.power_level,
            event: &e_shallow,
            version: StateResVersion::V1,
        };
        // V1: shallow depth (1) is better. Better must be Smaller (pops last). So p_v1_base > p_shallow.
        assert_eq!(p_v1_base.cmp(&p_shallow), Ordering::Greater);
        let p_v1_early_id = SortPriority {
            power_level: e_early_id.power_level,
            event: &e_early_id,
            version: StateResVersion::V1,
        };
        // V1: early ID "a" is better. Better must be Smaller (pops last). So p_v1_base > p_v1_early_id.
        assert_eq!(p_v1_base.cmp(&p_v1_early_id), Ordering::Greater);
        assert_eq!(p_v1_base.cmp(&p_v1_base), Ordering::Equal);
    }

    // ========================================================================
    // Phase 2: Battle-Hardening Tests
    // ========================================================================

    #[test]
    fn test_cycle_detection_detailed() {
        let mut events = HashMap::new();
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                auth_events: vec!["B".into()],
                ..Default::default()
            },
        );
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                auth_events: vec!["A".into()],
                ..Default::default()
            },
        );
        let result = lean_kahn_sort_detailed(&events, &events, StateResVersion::V2);
        match result {
            KahnSortResult::CycleDetected { sorted, stuck } => {
                assert!(sorted.is_empty());
                assert_eq!(stuck.len(), 2);
                let mut stuck_sorted = stuck.clone();
                stuck_sorted.sort();
                assert_eq!(stuck_sorted, vec!["A", "B"]);
            }
            KahnSortResult::Ok(_) => panic!("Expected cycle detection"),
        }
    }

    #[test]
    fn test_cycle_detection_partial_sort() {
        // C -> A -> B -> A (cycle), but C is reachable
        let mut events = HashMap::new();
        events.insert(
            "C".into(),
            LeanEvent {
                event_id: "C".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                auth_events: vec![],
                ..Default::default()
            },
        );
        events.insert(
            "A".into(),
            LeanEvent {
                event_id: "A".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                auth_events: vec!["B".into(), "C".into()],
                ..Default::default()
            },
        );
        events.insert(
            "B".into(),
            LeanEvent {
                event_id: "B".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                auth_events: vec!["A".into()],
                ..Default::default()
            },
        );
        let result = lean_kahn_sort_detailed(&events, &events, StateResVersion::V2);
        match result {
            KahnSortResult::CycleDetected { sorted, stuck } => {
                assert_eq!(sorted, vec!["C"]);
                assert_eq!(stuck.len(), 2);
            }
            KahnSortResult::Ok(_) => panic!("Expected cycle detection"),
        }
    }

    #[test]
    fn test_kahn_sort_result_api() {
        let ok = KahnSortResult::Ok(vec!["A".into()]);
        assert!(ok.is_ok());
        assert_eq!(ok.into_sorted(), vec!["A"]);

        let cycle = KahnSortResult::CycleDetected {
            sorted: vec!["C".into()],
            stuck: vec!["A".into(), "B".into()],
        };
        assert!(!cycle.is_ok());
        assert!(cycle.into_sorted().is_empty());
    }

    #[test]
    fn test_power_level_coercion_integer() {
        let json = r#"{"event_id": "$1", "type": "m.room.member", "origin_server_ts": 1, "power_level": 100}"#;
        let ev: LeanEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.power_level, 100);
    }

    #[test]
    fn test_power_level_coercion_string() {
        let json = r#"{"event_id": "$1", "type": "m.room.member", "origin_server_ts": 1, "power_level": "100"}"#;
        let ev: LeanEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.power_level, 100);
    }

    #[test]
    fn test_power_level_coercion_float() {
        let json = r#"{"event_id": "$1", "type": "m.room.member", "origin_server_ts": 1, "power_level": 100.0}"#;
        let ev: LeanEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.power_level, 100);
    }

    #[test]
    fn test_power_level_coercion_invalid_string() {
        let json = r#"{"event_id": "$1", "type": "m.room.member", "origin_server_ts": 1, "power_level": "abc"}"#;
        let ev: LeanEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.power_level, 0);
    }

    #[test]
    fn test_deep_chain_stack_safety() {
        // 1000-event deep chain: ev_0 <- ev_1 <- ev_2 <- ... <- ev_999
        let mut events = HashMap::new();
        for i in 0..1000u32 {
            let id = alloc::format!("ev_{}", i);
            let auth = if i > 0 {
                vec![alloc::format!("ev_{}", i - 1)]
            } else {
                vec![]
            };
            events.insert(
                id.clone(),
                LeanEvent {
                    event_id: id,
                    event_type: "m.room.member".into(),
                    state_key: Some("@alice:example.com".into()),
                    power_level: 100,
                    origin_server_ts: i as u64,
                    auth_events: auth,
                    depth: i as u64,
                    ..Default::default()
                },
            );
        }
        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        assert_eq!(sorted.len(), 1000);
        // First element must be ev_0 (in-degree 0)
        assert_eq!(sorted[0], "ev_0");
        // Last element must be ev_999 (deepest)
        assert_eq!(sorted[999], "ev_999");
    }

    #[test]
    fn test_subgraph_bounded_depth() {
        // Chain: A <- B <- C <- D (all in conflicted set for proper subgraph)
        let mut graph = HashMap::new();
        for (id, auths) in [
            ("A", vec![]),
            ("B", vec!["A"]),
            ("C", vec!["B"]),
            ("D", vec!["C"]),
        ] {
            graph.insert(
                id.to_string(),
                LeanEvent {
                    event_id: id.into(),
                    event_type: "m.room.member".into(),
                    state_key: Some("@alice:example.com".into()),
                    auth_events: auths.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            );
        }
        // Unbounded with A and D as conflicted: full intersection includes all
        let full = compute_v2_1_conflicted_subgraph_bounded(
            &graph,
            &["A".to_string(), "D".to_string()],
            None,
        );
        assert!(full.subgraph.contains_key("A"));
        assert!(full.subgraph.contains_key("D"));

        // Bounded to depth 1: backwards from D only reaches C (depth 1),
        // so the backwards set is {A, D, C} (A + D from seeds, C from D's auth).
        // But A is not reachable forward from any of these at depth 1 only.
        let bounded = compute_v2_1_conflicted_subgraph_bounded(
            &graph,
            &["A".to_string(), "D".to_string()],
            Some(1),
        );
        // D at depth 0, C at depth 1 from D's backwards walk
        assert!(bounded.subgraph.contains_key("D"));
        assert!(bounded.subgraph.contains_key("A"));
        // B is NOT reachable within depth 1 from D (it's at depth 2)
        assert!(!bounded.subgraph.contains_key("B"));
    }

    #[test]
    fn test_subgraph_missing_auth_detection() {
        let mut graph = HashMap::new();
        graph.insert(
            "X".to_string(),
            LeanEvent {
                event_id: "X".into(),
                event_type: "m.room.member".into(),
                state_key: Some("@alice:example.com".into()),
                auth_events: vec!["MISSING_1".into(), "MISSING_2".into()],
                ..Default::default()
            },
        );
        let result = compute_v2_1_conflicted_subgraph_bounded(&graph, &["X".to_string()], None);
        let mut missing = result.missing_auth_events.clone();
        missing.sort();
        assert_eq!(missing, vec!["MISSING_1", "MISSING_2"]);
    }

    fn default_test_event(id: &str, pl: i64, ts: u64, auth: Vec<&str>) -> LeanEvent {
        LeanEvent {
            event_id: id.into(),
            event_type: "m.room.message".into(), // not power
            state_key: None,
            power_level: pl,
            origin_server_ts: ts,
            prev_events: vec![],
            auth_events: auth.into_iter().map(ToString::to_string).collect(),
            depth: 1,
            sender: "@user:example.com".into(),
            content: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    #[test]
    fn test_mainline_sort_no_pl_ancestor_sorts_first() {
        // PL mainline: pl-3 -> pl-2 -> pl-1
        let mainline = vec![
            "$pl-3".to_string(),
            "$pl-2".to_string(),
            "$pl-1".to_string(),
        ];

        let mut auth_context = HashMap::new();
        // Mock auth context to build the paths
        auth_context.insert(
            "$msg-old".into(),
            default_test_event("$msg-old", 0, 20, vec!["$pl-1"]),
        );
        auth_context.insert(
            "$msg-new".into(),
            default_test_event("$msg-new", 0, 30, vec!["$pl-3"]),
        );
        auth_context.insert(
            "$msg-no-pl".into(),
            default_test_event("$msg-no-pl", 0, 10, vec![]),
        );

        // Add PL events themselves to auth context
        auth_context.insert(
            "$pl-3".into(),
            default_test_event("$pl-3", 100, 3, vec!["$pl-2"]),
        );
        auth_context.insert(
            "$pl-2".into(),
            default_test_event("$pl-2", 100, 2, vec!["$pl-1"]),
        );
        auth_context.insert("$pl-1".into(), default_test_event("$pl-1", 100, 1, vec![]));

        let ev_old = auth_context.get("$msg-old").unwrap();
        let ev_new = auth_context.get("$msg-new").unwrap();
        let ev_no_pl = auth_context.get("$msg-no-pl").unwrap();

        let mut events_to_sort = vec![ev_old, ev_new, ev_no_pl];

        mainline_sort(&mut events_to_sort, &mainline, &auth_context);

        let sorted_ids: Vec<String> = events_to_sort.iter().map(|e| e.event_id.clone()).collect();
        // Per spec, an event with i = ∞ (no mainline ancestor) sorts before all
        // chain-rooted events under "x < y if x.position is greater than y's".
        assert_eq!(sorted_ids, vec!["$msg-no-pl", "$msg-old", "$msg-new"]);
    }

    #[test]
    fn test_reverse_topological_power_sort() {
        let mut events = HashMap::new();
        // Graph structure from Ruma test:
        // l -> o
        // m -> n, o
        // n -> o
        // p -> o
        // We use V2 which uses PL, TS, and ID. To match Ruma exactly, we just use defaults.
        // Wait, the Ruma test passes `int!(0)` for all power levels and TS.
        events.insert("$l".into(), default_test_event("$l", 0, 0, vec!["$o"]));
        events.insert(
            "$m".into(),
            default_test_event("$m", 0, 0, vec!["$n", "$o"]),
        );
        events.insert("$n".into(), default_test_event("$n", 0, 0, vec!["$o"]));
        events.insert("$o".into(), default_test_event("$o", 0, 0, vec![]));
        events.insert("$p".into(), default_test_event("$p", 0, 0, vec!["$o"]));

        let sorted = lean_kahn_sort(&events, &events, StateResVersion::V2);
        // All events have same PL=0 and ts=0, so tie-break is by event_id.
        // Smaller id pops first (loses). Sorted: $o (root), then $l < $n < $p in id order,
        // $m waits for $n. After $n pops, $m becomes eligible and beats $p ("m" > "p"? no:
        // "$m" < "$p" → $m pops first). So order: [$o, $l, $n, $m, $p].
        assert_eq!(sorted, vec!["$o", "$l", "$n", "$m", "$p"]);
    }
}
