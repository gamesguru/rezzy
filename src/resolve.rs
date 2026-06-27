use crate::cdo::apply_cdo_filter;
use crate::sorting::{build_mainline, lean_kahn_sort, mainline_sort};
use crate::state_at::{compute_local_auth, iterative_auth_ok, LocalAuthCache};
use crate::types::{LeanEvent, StateResVersion};
use crate::HashMap;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

#[must_use]
pub fn resolve_lean<S1: core::hash::BuildHasher, S2: core::hash::BuildHasher>(
    unconflicted_state: BTreeMap<(String, Option<String>), String>,
    mut conflicted_events: HashMap<String, LeanEvent, S1>,
    auth_context: &HashMap<String, LeanEvent, S2>,
    version: StateResVersion,
) -> BTreeMap<(String, Option<String>), String> {
    if version == StateResVersion::V2_1_1 {
        let filtered = apply_cdo_filter(&conflicted_events, auth_context);
        conflicted_events.clear();
        for (k, v) in filtered {
            conflicted_events.insert(k, v);
        }
    }

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

    // MSC4297 (v2.1+): The algorithm starts from an empty set of state.
    let mut resolved = match version {
        StateResVersion::V2_1 | StateResVersion::V2_1_1 | StateResVersion::V2_2 => BTreeMap::new(),
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

    let create_ev = auth_context
        .values()
        .chain(sort_set.values())
        .find(|ev| ev.event_type == "m.room.create");

    // Step 1: Sort power events by reverse topological power ordering (Kahn sort)
    // Step 2: Apply iterative auth checks (per spec & Ruma implementation)
    let mut local_auth_cache: LocalAuthCache = HashMap::new();

    let sorted_power_ids = lean_kahn_sort(&power_events, &sort_context, create_ev, version);
    for id in &sorted_power_ids {
        if let Some(event) = sort_set.get(id) {
            let local_auth = compute_local_auth(
                event,
                auth_context,
                sort_set,
                &mut local_auth_cache,
                version,
            );
            if iterative_auth_ok(
                event,
                &resolved,
                auth_context,
                sort_set,
                local_auth,
                create_ev,
                version,
                true,
            ) {
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
        let local_auth =
            compute_local_auth(ev, auth_context, sort_set, &mut local_auth_cache, version);
        if iterative_auth_ok(
            ev,
            &resolved,
            auth_context,
            sort_set,
            local_auth,
            create_ev,
            version,
            false,
        ) {
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
    drop(conflicted_events);
    final_resolved
}
