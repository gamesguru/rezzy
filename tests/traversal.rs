use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

#[test]
fn test_v2_1_vs_v2_2_recursive_auth_lookup() {
    // SCENARIO: A state event requires a Power Level that is 2 hops away.
    // Event E (Room Name) -> Auth: [Member Alice: join]
    // Member Alice -> Auth: [Power Levels PL] (Alice has PL 100)
    //
    // Critically, Event E OMITTED the Power Level event from its own auth_events.
    //
    // V2.1 (1-hop): Only sees the join. Misses the PL event.
    // Alice's PL defaults to 0. State events require PL 50.
    // Result: REJECTED.
    //
    // V2.2 (Recursive BFS): Walks back from the join, finds the PL event.
    // Alice has PL 100. 100 >= 50.
    // Result: ACCEPTED.

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

    let alice_join = LeanEvent {
        event_id: "$join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@alice:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 300,
        content: json!({ "membership": "join" }),
        // Join includes PL to be authorized
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
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
    assert!(
        !resolved_v21.contains_key(&("m.room.name".to_string(), Some("".to_string()))),
        "V2.1 should have rejected the name change due to missing PL event in local 1-hop auth"
    );

    // V2.2: Should SUCCEED.
    // It heals the chain by finding $pl as an ancestor of $join.
    let resolved_v22 = resolve_lean(
        BTreeMap::new(),
        conflicted_events,
        &auth_context,
        StateResVersion::V2_2,
    );
    assert!(
        resolved_v22.contains_key(&("m.room.name".to_string(), Some("".to_string()))),
        "V2.2 should have accepted the name change by finding the PL event via BFS"
    );
    assert_eq!(
        resolved_v22.get(&("m.room.name".to_string(), Some("".to_string()))),
        Some(&"$name".to_string())
    );
}
