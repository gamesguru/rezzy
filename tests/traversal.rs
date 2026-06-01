use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

#[test]
fn test_v2_1_vs_v2_2_recursive_auth_lookup() {
    // SCENARIO: An event requires a Power Level event that is 2 hops away in the auth chain.
    // Event E -> Auth: [Member M]
    // Member M -> Auth: [PowerLevels PL]
    //
    // V2.1 (1-hop) should fail to find PL (if PL isn't in resolved state).
    // V2.2 (Recursive) should find PL.

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
            "users": { "@alice:example.com": 100 }
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let alice_member = LeanEvent {
        event_id: "$alice_member".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@alice:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 300,
        content: json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    // This message is only valid if Alice has PL 100 (from $pl).
    // It auths via Alice's membership.
    let alice_msg = LeanEvent {
        event_id: "$msg".to_string(),
        event_type: "m.room.message".to_string(),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 400,
        auth_events: vec!["$create".to_string(), "$alice_member".to_string()],
        ..Default::default()
    };

    let mut auth_context = HashMap::new();
    auth_context.insert(create_ev.event_id.clone(), create_ev);
    auth_context.insert(pl_ev.event_id.clone(), pl_ev);
    auth_context.insert(alice_member.event_id.clone(), alice_member);

    let mut conflicted_events = HashMap::new();
    conflicted_events.insert(alice_msg.event_id.clone(), alice_msg);

    // V2.1: Should FAIL to resolve the message because it can't find $pl.
    // It looks at $alice_member (1 hop), sees it needs PL to check if Alice can send,
    // but $pl is 2 hops away from $msg (it's an auth-event of $alice_member).
    // Since resolved state is empty (MSC4297 start), it has no fallback.
    let resolved_v21 = resolve_lean(
        BTreeMap::new(),
        conflicted_events.clone(),
        &auth_context,
        StateResVersion::V2_1,
    );
    assert!(!resolved_v21.contains_key(&("m.room.message".to_string(), None)));

    // V2.2: Should SUCCEED because it performs a recursive BFS walk.
    // It finds $alice_member, then finds $pl as an ancestor, and correctly auths the message.
    let resolved_v22 = resolve_lean(
        BTreeMap::new(),
        conflicted_events,
        &auth_context,
        StateResVersion::V2_2,
    );
    assert!(resolved_v22.contains_key(&("m.room.message".to_string(), None)));
    assert_eq!(
        resolved_v22.get(&("m.room.message".to_string(), None)),
        Some(&"$msg".to_string())
    );
}
