use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

fn run_auth_lookup_scenario(
    join_auth_includes_pl: bool,
    expected_v21_success: bool,
    expected_v22_success: bool,
) {
    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@creator:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_ev = LeanEvent {
        event_id: "$pl".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@creator:example.com".to_string(),
        origin_server_ts: 200,
        content: json!({
            "users": { "@alice:example.com": 100 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let mut join_auth = vec!["$create".to_string()];
    if join_auth_includes_pl {
        join_auth.push("$pl".to_string());
    }

    let alice_join = LeanEvent {
        event_id: "$join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@alice:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 300,
        content: json!({ "membership": "join" }),
        auth_events: join_auth,
        ..Default::default()
    };

    // The name event. It requires PL 50.
    // It lists the join in auth_events, but OMITS the PL event.
    let alice_name = LeanEvent {
        event_id: "$name".to_string(),
        event_type: "m.room.name".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 400,
        content: json!({ "name": "Alice's Room" }),
        // OMIT the PL event directly. It's only 1-hop if we put it here, which we don't.
        auth_events: vec!["$create".to_string(), "$join".to_string()],
        ..Default::default()
    };

    let mut auth_context = HashMap::new();
    auth_context.insert(create_ev.event_id.clone(), create_ev);
    auth_context.insert(pl_ev.event_id.clone(), pl_ev);
    auth_context.insert(alice_join.event_id.clone(), alice_join);

    let mut conflicted_events = HashMap::new();
    conflicted_events.insert(alice_name.event_id.clone(), alice_name);

    // V2.1: Should FAIL to resolve the name change.
    // It doesn't see the PL event, so it uses default PL 0 for Alice.
    let resolved_v21 = resolve_lean(
        BTreeMap::new(),
        conflicted_events.clone(),
        &auth_context,
        StateResVersion::V2_1,
    );
    let v21_success = resolved_v21.contains_key(&("m.room.name".to_string(), Some("".to_string())));
    assert_eq!(
        v21_success, expected_v21_success,
        "V2.1 success expectation mismatched: got {v21_success}, expected {expected_v21_success}"
    );

    let resolved_v22 = resolve_lean(
        BTreeMap::new(),
        conflicted_events,
        &auth_context,
        StateResVersion::V2_2,
    );
    let v22_success = resolved_v22.contains_key(&("m.room.name".to_string(), Some("".to_string())));
    assert_eq!(
        v22_success, expected_v22_success,
        "V2.2 success expectation mismatched: got {v22_success}, expected {expected_v22_success}"
    );
}

#[test]
fn test_v2_1_vs_v2_2_recursive_auth_lookup() {
    // Join event includes PL. PL is in the auth ancestry (depth 2).
    // V2.1 fails because it only checks 1-hop (depth 1).
    // V2.2 succeeds because BFS finds the PL in ancestry.
    run_auth_lookup_scenario(true, false, true);
}

#[test]
fn test_v2_2_xfail_disconnected_auth() {
    // Join event DOES NOT include PL. PL is disconnected from auth graph.
    // V2.1 fails.
    // V2.2 also fails, correctly expected.
    run_auth_lookup_scenario(false, false, false);
}

#[test]
fn test_v2_2_deep_auth_chain_101() {
    // SCENARIO: The required Power Level event is 101 hops deep in the auth chain.
    // We create a linear auth chain of 101 events: E_101 -> E_100 -> ... -> E_1 -> PL
    // The final event E_final only lists E_101 in its auth_events.
    // We want to verify if V2.2's BFS can traverse all 101 hops to find the PL event,
    // or if a depth limit prevents it.

    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@creator:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_ev = LeanEvent {
        event_id: "$pl".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@creator:example.com".to_string(),
        origin_server_ts: 200,
        content: serde_json::json!({
            "users": { "@alice:example.com": 100 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let mut auth_context = HashMap::new();
    auth_context.insert(create_ev.event_id.clone(), create_ev.clone());
    auth_context.insert(pl_ev.event_id.clone(), pl_ev.clone());

    let mut last_event_id = "$pl".to_string();
    for i in 1..=101 {
        let ev_id = format!("$dummy_{}", i);
        let ev = LeanEvent {
            event_id: ev_id.clone(),
            event_type: "m.dummy".to_string(),
            state_key: Some(format!("state_{}", i)),
            sender: "@alice:example.com".to_string(),
            origin_server_ts: 200 + i as u64,
            auth_events: vec!["$create".to_string(), last_event_id.clone()],
            ..Default::default()
        };
        auth_context.insert(ev_id.clone(), ev);
        last_event_id = ev_id;
    }

    let alice_join = LeanEvent {
        event_id: "$join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@alice:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 900,
        content: serde_json::json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), last_event_id.clone()],
        ..Default::default()
    };
    auth_context.insert(alice_join.event_id.clone(), alice_join.clone());

    let alice_name = LeanEvent {
        event_id: "$name".to_string(),
        event_type: "m.room.name".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 1000,
        content: serde_json::json!({ "name": "Alice's Room" }),
        auth_events: vec!["$create".to_string(), alice_join.event_id.clone()],
        ..Default::default()
    };

    let mut conflicted_events = HashMap::new();
    conflicted_events.insert(alice_name.event_id.clone(), alice_name);

    let resolved_v21 = resolve_lean(
        BTreeMap::new(),
        conflicted_events.clone(),
        &auth_context,
        StateResVersion::V2_1,
    );
    assert!(
        !resolved_v21.contains_key(&("m.room.name".to_string(), Some("".to_string()))),
        "V2.1 should have failed 101 hops deep"
    );

    let resolved_v22 = resolve_lean(
        BTreeMap::new(),
        conflicted_events,
        &auth_context,
        StateResVersion::V2_2,
    );
    assert!(
        resolved_v22.contains_key(&("m.room.name".to_string(), Some("".to_string()))),
        "V2.2 should have found the PL event 101 hops deep!"
    );
}

#[test]
fn test_v2_2_performance_1_million_hops() {
    // SCENARIO: A massive auth chain of 1,000,000 events.
    // This proves that the BFS traversal and HashMap lookups can handle
    // gigantic DAGs without stack overflows (since it's iterative) and
    // without taking an unreasonable amount of time.

    use std::time::Instant;
    let start_setup = Instant::now();

    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@creator:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_ev = LeanEvent {
        event_id: "$pl".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@creator:example.com".to_string(),
        origin_server_ts: 200,
        content: serde_json::json!({
            "users": { "@alice:example.com": 100 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let mut auth_context = HashMap::with_capacity(1_000_005);
    auth_context.insert(create_ev.event_id.clone(), create_ev.clone());
    auth_context.insert(pl_ev.event_id.clone(), pl_ev.clone());

    let mut last_event_id = "$pl".to_string();
    for i in 1..=1_000_000 {
        let ev_id = format!("$dummy_{}", i);
        let ev = LeanEvent {
            event_id: ev_id.clone(),
            event_type: "m.dummy".to_string(),
            state_key: None,
            sender: "@alice:example.com".to_string(),
            origin_server_ts: 200 + i as u64,
            auth_events: vec!["$create".to_string(), last_event_id],
            ..Default::default()
        };
        last_event_id = ev_id.clone();
        auth_context.insert(ev_id, ev);
    }

    let alice_join = LeanEvent {
        event_id: "$join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@alice:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 2000000,
        content: serde_json::json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), last_event_id.clone()],
        ..Default::default()
    };
    auth_context.insert(alice_join.event_id.clone(), alice_join.clone());

    let alice_name = LeanEvent {
        event_id: "$name".to_string(),
        event_type: "m.room.name".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 2000001,
        content: serde_json::json!({ "name": "Alice's Room" }),
        auth_events: vec!["$create".to_string(), alice_join.event_id.clone()],
        ..Default::default()
    };

    let mut conflicted_events = HashMap::new();
    conflicted_events.insert(alice_name.event_id.clone(), alice_name);

    println!("Setup 1,000,000 events took: {:?}", start_setup.elapsed());

    let start_resolve = Instant::now();
    let resolved_v22 = resolve_lean(
        BTreeMap::new(),
        conflicted_events,
        &auth_context,
        StateResVersion::V2_2,
    );
    let resolve_duration = start_resolve.elapsed();
    println!(
        "State Resolution (V2.2) of 1,000,000 hops took: {:?}",
        resolve_duration
    );

    assert!(
        resolved_v22.contains_key(&("m.room.name".to_string(), Some("".to_string()))),
        "V2.2 should have found the PL event 1,000,000 hops deep!"
    );
}

#[test]
fn test_v2_2_ancient_prev_event_allowed() {
    // SCENARIO: Alice sends a state event (m.room.name) where her client
    // sets `prev_events` to the VERY FIRST event in the room ($create),
    // effectively skipping the entire timeline graph.
    // This proves that State Resolution doesn't care about `prev_events`.

    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@creator:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_ev = LeanEvent {
        event_id: "$pl".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@creator:example.com".to_string(),
        origin_server_ts: 200,
        content: serde_json::json!({
            "users": { "@alice:example.com": 100 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let alice_join = LeanEvent {
        event_id: "$join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@alice:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 300,
        content: serde_json::json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let mut auth_context = HashMap::new();
    auth_context.insert(create_ev.event_id.clone(), create_ev.clone());
    auth_context.insert(pl_ev.event_id.clone(), pl_ev.clone());
    auth_context.insert(alice_join.event_id.clone(), alice_join.clone());

    // Alice changes the room name, but references the ancient $create event in prev_events.
    let alice_name = LeanEvent {
        event_id: "$name".to_string(),
        event_type: "m.room.name".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 1000,
        content: serde_json::json!({ "name": "Alice's Room" }),
        auth_events: vec![
            "$create".to_string(),
            "$join".to_string(),
            "$pl".to_string(),
        ],
        prev_events: vec!["$create".to_string()], // <-- Ancient prev_event!
        ..Default::default()
    };

    let mut conflicted_events = HashMap::new();
    conflicted_events.insert(alice_name.event_id.clone(), alice_name);

    let resolved_v22 = resolve_lean(
        BTreeMap::new(),
        conflicted_events,
        &auth_context,
        StateResVersion::V2_2,
    );

    // State resolution still passes because the auth_events are valid.
    assert!(
        resolved_v22.contains_key(&("m.room.name".to_string(), Some("".to_string()))),
        "V2.2 should allow the event even with an ancient prev_event"
    );
}
