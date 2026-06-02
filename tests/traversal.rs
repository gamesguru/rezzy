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
    // User expects V2.2 to pass despite the missing auth link.
    run_auth_lookup_scenario(false, false, true);
}
