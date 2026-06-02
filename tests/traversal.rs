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

#[test]
fn test_kahn_tiebreak_power_level_overwrites_via_auth() {
    // This test explicitly proves how the tie-breaker works for Power Events.
    // High Power Levels pop FIRST in Kahn's sort. Wait, if they pop first, don't they lose to Last-Write-Wins? 
    // No! Power Events are special. They set the authorization rules for the rest of the loop! 
    // If Alice (PL 100) bans Bob, her event pops first and sets the ban in the state map.
    // When Bob's conflicting PL 0 join pops later, the state already contains the ban. 
    // `iterative_auth_ok` evaluates Bob's join, sees he is banned, and completely rejects his event. 
    // So Alice's ban stays.

    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_ev = LeanEvent {
        event_id: "$pl".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 200,
        content: json!({
            "users": { "@alice:example.com": 100 },
            "events_default": 0,
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    // Alice (PL 100) bans Bob.
    let alice_ban = LeanEvent {
        event_id: "$alice_ban".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@bob:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 300,
        content: json!({ "membership": "ban" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    // Bob (PL 0) attempts to join.
    // Exact same origin_server_ts and auth_chain_distance as the ban to force a pure Power Level tie-break.
    let bob_join = LeanEvent {
        event_id: "$bob_join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@bob:example.com".to_string()),
        sender: "@bob:example.com".to_string(),
        origin_server_ts: 300, 
        content: json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let mut auth_context = HashMap::new();
    auth_context.insert(create_ev.event_id.clone(), create_ev);
    auth_context.insert(pl_ev.event_id.clone(), pl_ev);

    let mut conflicted_events = HashMap::new();
    conflicted_events.insert(alice_ban.event_id.clone(), alice_ban);
    conflicted_events.insert(bob_join.event_id.clone(), bob_join);

    let resolved = resolve_lean(
        BTreeMap::new(),
        conflicted_events,
        &auth_context,
        StateResVersion::V2_2,
    );

    // The resolved state should contain the ban, not the join
    let member_key = ("m.room.member".to_string(), Some("@bob:example.com".to_string()));
    assert_eq!(
        resolved.get(&member_key).unwrap(),
        "$alice_ban",
        "Alice's ban should win against Bob's concurrent join because her higher PL forces it to pop first, setting the auth rules."
    );
}

#[test]
fn test_kahn_tiebreak_mods_banning_each_other() {
    // Two mods (both PL 50) ban each other concurrently.
    // They tie for Power Level.
    // They have different state keys (Alice bans Bob, Bob bans Alice), so they don't overwrite each other.
    // Kahn's sort determines who pops FIRST based on event_id.
    // Whoever pops FIRST sets their ban in the state.
    // When the second person's ban is evaluated, `iterative_auth_ok` sees they are ALREADY banned.
    // Therefore, the second person's ban is REJECTED.
    // "Who shoots first wins."

    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_ev = LeanEvent {
        event_id: "$pl".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 200,
        content: json!({
            "users": { "@alice:example.com": 50, "@bob:example.com": 50 },
            "events_default": 0,
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    // Alice bans Bob.
    // event_id = "$A_alice_ban"
    let alice_ban = LeanEvent {
        event_id: "$A_alice_ban".to_string(), 
        event_type: "m.room.member".to_string(),
        state_key: Some("@bob:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 300,
        content: json!({ "membership": "ban" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    // Bob bans Alice.
    // event_id = "$Z_bob_ban"
    let bob_ban = LeanEvent {
        event_id: "$Z_bob_ban".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@alice:example.com".to_string()),
        sender: "@bob:example.com".to_string(),
        origin_server_ts: 300, 
        content: json!({ "membership": "ban" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let mut auth_context = HashMap::new();
    auth_context.insert(create_ev.event_id.clone(), create_ev);
    auth_context.insert(pl_ev.event_id.clone(), pl_ev);

    let mut conflicted_events = HashMap::new();
    conflicted_events.insert(alice_ban.event_id.clone(), alice_ban);
    conflicted_events.insert(bob_ban.event_id.clone(), bob_ban);

    let resolved = resolve_lean(
        BTreeMap::new(),
        conflicted_events,
        &auth_context,
        StateResVersion::V2_2,
    );

    // Let's figure out who pops first!
    // Priority: other.event.event_id.cmp(&self.event.event_id)
    // Alice = "$A", Bob = "$Z"
    // Alice's cmp: "$Z".cmp("$A") -> Greater. Alice is Greater, pops FIRST.
    // Bob's cmp: "$A".cmp("$Z") -> Less. Bob is Less, pops LAST.
    // Wait! Under State Resolution V2.1 and V2.2, auth checks for non-power-level events
    // are strictly isolated to their own `auth_events` chain! (This is the core fix of MSC4297).
    // Because neither ban is in the other's auth chain, NEITHER sees the other's ban during `iterative_auth_ok`!
    // Therefore, BOTH bans pass authorization! 
    // And since they have different state keys (@bob vs @alice), they both get inserted!
    // Result: Mutual Destruction.

    let bob_member_key = ("m.room.member".to_string(), Some("@bob:example.com".to_string()));
    let alice_member_key = ("m.room.member".to_string(), Some("@alice:example.com".to_string()));

    assert_eq!(
        resolved.get(&bob_member_key).unwrap(),
        "$A_alice_ban",
        "Bob should be banned because Alice's ban was authorized by its local auth chain."
    );

    assert_eq!(
        resolved.get(&alice_member_key).unwrap(),
        "$Z_bob_ban",
        "Alice should ALSO be banned because Bob's ban was authorized by its local auth chain! (V2.1 isolated auth)"
    );
}
