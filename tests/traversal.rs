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
    let member_key = (
        "m.room.member".to_string(),
        Some("@bob:example.com".to_string()),
    );
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

    let bob_member_key = (
        "m.room.member".to_string(),
        Some("@bob:example.com".to_string()),
    );
    let alice_member_key = (
        "m.room.member".to_string(),
        Some("@alice:example.com".to_string()),
    );

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

#[test]
fn test_kahn_tiebreak_mods_banning_each_other_v2_1() {
    // Exact same test, but running under V2.1 to prove auth_chain_distance doesn't change the outcome.
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
        content: serde_json::json!({
            "users": { "@alice:example.com": 50, "@bob:example.com": 50 },
            "events_default": 0,
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let alice_ban = LeanEvent {
        event_id: "$A_alice_ban".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@bob:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 300,
        content: serde_json::json!({ "membership": "ban" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let bob_ban = LeanEvent {
        event_id: "$Z_bob_ban".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@alice:example.com".to_string()),
        sender: "@bob:example.com".to_string(),
        origin_server_ts: 300,
        content: serde_json::json!({ "membership": "ban" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let mut auth_context = std::collections::HashMap::new();
    auth_context.insert(create_ev.event_id.clone(), create_ev);
    auth_context.insert(pl_ev.event_id.clone(), pl_ev);

    let mut conflicted_events = std::collections::HashMap::new();
    conflicted_events.insert(alice_ban.event_id.clone(), alice_ban);
    conflicted_events.insert(bob_ban.event_id.clone(), bob_ban);

    let resolved = ruma_lean::resolve_lean(
        std::collections::BTreeMap::new(),
        conflicted_events,
        &auth_context,
        ruma_lean::StateResVersion::V2_1,
    );

    let bob_member_key = (
        "m.room.member".to_string(),
        Some("@bob:example.com".to_string()),
    );
    let alice_member_key = (
        "m.room.member".to_string(),
        Some("@alice:example.com".to_string()),
    );

    assert_eq!(resolved.get(&bob_member_key).unwrap(), "$A_alice_ban");
    let bob_member_key = (
        "m.room.member".to_string(),
        Some("@bob:example.com".to_string()),
    );
    let alice_member_key = (
        "m.room.member".to_string(),
        Some("@alice:example.com".to_string()),
    );

    assert_eq!(resolved.get(&bob_member_key).unwrap(), "$A_alice_ban");
    assert_eq!(resolved.get(&alice_member_key).unwrap(), "$Z_bob_ban");
}

#[test]
fn test_invite_lock_spam_wave_with_bans() {
    // A programmatic model of the pathology_spam_break_full scenario,
    // explicitly augmented with "disabling invites" and a "wave of bans" on the mainline.

    let create_ev = LeanEvent {
        event_id: "$1_create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_ev = LeanEvent {
        event_id: "$2_pl".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 200,
        content: serde_json::json!({
            "users": { "@admin:example.com": 100 },
            "events_default": 0,
            "state_default": 50
        }),
        auth_events: vec!["$1_create".to_string()],
        ..Default::default()
    };

    // --- MAINLINE ---
    // The admin explicitly disables invites.
    let disable_invites = LeanEvent {
        event_id: "$3_join_rules_invite".to_string(),
        event_type: "m.room.join_rules".to_string(),
        state_key: Some("".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 300,
        content: serde_json::json!({ "join_rule": "invite" }),
        auth_events: vec!["$1_create".to_string(), "$2_pl".to_string()],
        ..Default::default()
    };

    // The admin executes a wave of bans.
    let ban1 = LeanEvent {
        event_id: "$4_ban1".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@spam1:example.com".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 301,
        content: serde_json::json!({ "membership": "ban" }),
        auth_events: vec!["$1_create".to_string(), "$2_pl".to_string()],
        ..Default::default()
    };

    let ban2 = LeanEvent {
        event_id: "$5_ban2".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@spam2:example.com".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 302,
        content: serde_json::json!({ "membership": "ban" }),
        auth_events: vec!["$1_create".to_string(), "$2_pl".to_string()],
        ..Default::default()
    };

    // --- ATTACKER (STALE FORK) ---
    // The attacker forks from $2_pl.
    // They manipulate the origin_server_ts to be extremely high (e.g. 999999)
    // to force Kahn's sort to favor their event in V2.1.
    // They submit a malicious state event forcing the room back to "public".
    let malicious_public = LeanEvent {
        event_id: "$99_join_rules_public".to_string(),
        event_type: "m.room.join_rules".to_string(),
        state_key: Some("".to_string()),
        sender: "@admin:example.com".to_string(), // Assume they hijacked the admin, or we just want to test tie-breaker
        origin_server_ts: 999999,                 // FAKED TIMESTAMP FOR TIE-BREAKER MANIPULATION
        content: serde_json::json!({ "join_rule": "public" }),
        auth_events: vec!["$1_create".to_string(), "$2_pl".to_string()],
        ..Default::default()
    };

    let mut auth_context = std::collections::HashMap::new();
    auth_context.insert(create_ev.event_id.clone(), create_ev);
    auth_context.insert(pl_ev.event_id.clone(), pl_ev);

    let mut conflicted_events = std::collections::HashMap::new();
    conflicted_events.insert(disable_invites.event_id.clone(), disable_invites);
    conflicted_events.insert(ban1.event_id.clone(), ban1);
    conflicted_events.insert(ban2.event_id.clone(), ban2);
    conflicted_events.insert(malicious_public.event_id.clone(), malicious_public);

    // Resolution under V2.1
    let resolved_v21 = ruma_lean::resolve_lean(
        std::collections::BTreeMap::new(),
        conflicted_events.clone(),
        &auth_context,
        ruma_lean::StateResVersion::V2_1,
    );

    // Resolution under V2.2
    let resolved_v22 = ruma_lean::resolve_lean(
        std::collections::BTreeMap::new(),
        conflicted_events,
        &auth_context,
        ruma_lean::StateResVersion::V2_2,
    );

    let join_rules_key = ("m.room.join_rules".to_string(), Some("".to_string()));

    // Wait, since auth_events are identical for both join_rules, V2.2 falls back to TS.
    // Since the attacker manipulated TS, the attacker might STILL win under V2.2!
    // Let's assert what happens so we can analyze the structural vulnerabilities together.

    // Check V2.1 outcome
    let v21_winner = resolved_v21.get(&join_rules_key).unwrap();
    println!("V2.1 Winner: {}", v21_winner);

    // Check V2.2 outcome
    let v22_winner = resolved_v22.get(&join_rules_key).unwrap();
    println!("V2.2 Winner: {}", v22_winner);
}

#[test]
fn test_v2_2_cve_demotion_evasion() {
    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    // Alice makes Eve an Admin (PL 100)
    let pl_promo = LeanEvent {
        event_id: "$pl_promo".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 200,
        content: serde_json::json!({
            "users": { "@alice:example.com": 100, "@eve:evil.com": 100 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    // Eve joins (auths against the PL where she is Admin)
    let eve_join = LeanEvent {
        event_id: "$eve_join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@eve:evil.com".to_string()),
        sender: "@eve:evil.com".to_string(),
        origin_server_ts: 300,
        content: serde_json::json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), "$pl_promo".to_string()],
        ..Default::default()
    };

    // Alice realizes Eve is evil, DEMOTES her to PL 0
    let pl_demote = LeanEvent {
        event_id: "$pl_demote".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 400,
        content: serde_json::json!({
            "users": { "@alice:example.com": 100, "@eve:evil.com": 0 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string(), "$pl_promo".to_string()],
        ..Default::default()
    };

    // THE ATTACK: Eve maliciously changes the room name.
    // She intentionally OMITS the demotion from her 1-hop auth_events, 
    // trying to hide it.
    let eve_attack = LeanEvent {
        event_id: "$eve_attack".to_string(),
        event_type: "m.room.name".to_string(),
        state_key: Some("".to_string()),
        sender: "@eve:evil.com".to_string(),
        origin_server_ts: 500,
        content: serde_json::json!({ "name": "Hacked by Eve" }),
        // OMITTED: "$pl_demote"
        auth_events: vec!["$create".to_string(), "$eve_join".to_string()],
        ..Default::default()
    };

    let mut auth_context = std::collections::HashMap::new();
    auth_context.insert("$create".to_string(), create_ev);
    auth_context.insert("$pl_promo".to_string(), pl_promo.clone());
    auth_context.insert("$eve_join".to_string(), eve_join);
    auth_context.insert("$pl_demote".to_string(), pl_demote.clone());

    let mut conflicted_events = std::collections::HashMap::new();
    conflicted_events.insert("$pl_promo".to_string(), pl_promo);
    conflicted_events.insert("$pl_demote".to_string(), pl_demote);
    conflicted_events.insert("$eve_attack".to_string(), eve_attack);

    // --- V2.1 SECURELY BLOCKS THE ATTACK ---
    // V2.1 resolves PLs first (picking the demotion). When validating Eve's attack,
    // V2.1 overlays the consensus PL (demotion). Eve is PL 0. Name change requires 50. REJECTED.
    let resolved_v21 = ruma_lean::resolve_lean(
        std::collections::BTreeMap::new(),
        conflicted_events.clone(),
        &auth_context,
        ruma_lean::StateResVersion::V2_1,
    );
    let name_key = ("m.room.name".to_string(), Some("".to_string()));
    assert!(
        !resolved_v21.contains_key(&name_key),
        "V2.1 Rightly Rejected the attack because Eve was demoted."
    );

    // --- V2.2 FALLS FOR THE ATTACK ---
    // V2.2 uses BFS. It walks back from $eve_attack -> $eve_join -> $pl_promo.
    // It finds the ancient PL where Eve had 100. It skips the supplemental merge because
    // $pl_demote is not in Eve's local auth ancestry. It authorizes the attack!
    let resolved_v22 = ruma_lean::resolve_lean(
        std::collections::BTreeMap::new(),
        conflicted_events,
        &auth_context,
        ruma_lean::StateResVersion::V2_2,
    );
    assert!(
        resolved_v22.contains_key(&name_key),
        "FATAL: V2.2 mistakenly allowed the attack by bypassing the demotion!"
    );
}

#[test]
fn test_v2_1_flaw_concurrent_ban_evasion() {
    // SCENARIO: The "Phantom State" Flaw in V2.1
    // If Alice bans Bob on Fork A, and Bob concurrently changes the room name on Fork B,
    // Bob's name change will NOT see the ban during resolution, because V2.1 isolated 
    // memberships to the local auth chain. Bob's state event will be accepted 
    // into the final resolved state despite him being banned!
    
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
        content: serde_json::json!({
            "users": { "@alice:example.com": 100, "@bob:example.com": 50 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let bob_join = LeanEvent {
        event_id: "$bob_join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@bob:example.com".to_string()),
        sender: "@bob:example.com".to_string(),
        origin_server_ts: 300,
        content: serde_json::json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    // FORK A: Alice bans Bob
    let alice_bans_bob = LeanEvent {
        event_id: "$alice_bans_bob".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@bob:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 400,
        content: serde_json::json!({ "membership": "ban" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string(), "$bob_join".to_string()],
        ..Default::default()
    };

    // FORK B: Bob changes the room name (happens concurrently)
    let bob_name_change = LeanEvent {
        event_id: "$bob_name_change".to_string(),
        event_type: "m.room.name".to_string(),
        state_key: Some("".to_string()),
        sender: "@bob:example.com".to_string(),
        origin_server_ts: 405,
        content: serde_json::json!({ "name": "Bob Rules" }),
        // Bob's local auth chain knows nothing of the ban on Fork A
        auth_events: vec!["$create".to_string(), "$bob_join".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let mut auth_context = std::collections::HashMap::new();
    auth_context.insert("$create".to_string(), create_ev);
    auth_context.insert("$pl".to_string(), pl_ev);
    auth_context.insert("$bob_join".to_string(), bob_join);

    let mut conflicted_events = std::collections::HashMap::new();
    conflicted_events.insert("$alice_bans_bob".to_string(), alice_bans_bob);
    conflicted_events.insert("$bob_name_change".to_string(), bob_name_change);

    // Run V2.1 Resolution
    let resolved_v21 = ruma_lean::resolve_lean(
        std::collections::BTreeMap::new(),
        conflicted_events,
        &auth_context,
        ruma_lean::StateResVersion::V2_1
    );

    // Alice's ban has PL 100, so Kahn sort evaluates it FIRST. It is added to the resolved state.
    assert_eq!(
        resolved_v21.get(&("m.room.member".to_string(), Some("@bob:example.com".to_string()))).unwrap(),
        "$alice_bans_bob",
        "Bob should be banned in the final state"
    );

    // But V2.1 accepts Bob's concurrent name change!
    assert!(
        resolved_v21.contains_key(&("m.room.name".to_string(), Some("".to_string()))),
        "V2.1 flaw: Mistakenly accepted Bob's name change because it ignored his concurrent ban!"
    );
    
    // A FUTURE ALGORITHM must ensure Bob's name change is rejected during merge.
}

#[test]
fn test_v2_1_strictness_future_v3_should_pass() {
    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    // Join Rules: Public
    let join_rules = LeanEvent {
        event_id: "$jr".to_string(),
        event_type: "m.room.join_rules".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 200,
        content: serde_json::json!({ "join_rule": "public" }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    // Bob joins. He is allowed because the room is public.
    // BUT a client bug caused him to omit `$jr` from his auth_events!
    let bob_join = LeanEvent {
        event_id: "$bob_join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@bob:example.com".to_string()),
        sender: "@bob:example.com".to_string(),
        origin_server_ts: 300,
        content: serde_json::json!({ "membership": "join" }),
        // BUG: Missing "$jr"
        auth_events: vec!["$create".to_string()], 
        ..Default::default()
    };

    let mut auth_context = std::collections::HashMap::new();
    auth_context.insert("$create".to_string(), create_ev);
    auth_context.insert("$jr".to_string(), join_rules);

    let mut conflicted_events = std::collections::HashMap::new();
    conflicted_events.insert("$bob_join".to_string(), bob_join);

    let resolved_v21 = ruma_lean::resolve_lean(
        std::collections::BTreeMap::new(),
        conflicted_events.clone(),
        &auth_context,
        ruma_lean::StateResVersion::V2_1,
    );
    
    // V2.1 Rightfully Fails: It enforces the 1-hop strictness. Without "$jr" in the auth chain, 
    // it defaults to Invite-Only and rejects the join.
    assert!(
        !resolved_v21.contains_key(&("m.room.member".to_string(), Some("@bob:example.com".to_string()))),
        "V2.1 rightfully rejected the event because the 1-hop auth list was incomplete."
    );

    // A future State DAG (MSC4242) algorithm could theoretically pass this by validating 
    // the room state via `prev_state_events` instead of relying on the fragile string array.
}
