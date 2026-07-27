use rezzy::resolve_state::multi::resolve_state_maps as namespaced_resolve_state_maps;
use rezzy::{resolve_state_maps, LeanEvent, StateResVersion};

fn make_event(
    event_id: &str,
    event_type: &str,
    state_key: &str,
    sender: &str,
    auth_events: Vec<&str>,
) -> LeanEvent<String> {
    LeanEvent {
        event_id: event_id.to_string(),
        event_type: event_type.to_string(),
        state_key: Some(state_key.to_string()),
        sender: sender.to_string(),
        origin_server_ts: 0,
        auth_events: auth_events.into_iter().map(str::to_string).collect(),
        content: serde_json::json!({}),
        ..Default::default()
    }
}

#[test]
fn resolve_state_namespace_alias_matches_root_api() {
    let create = make_event("$create", "m.room.create", "", "@alice:x", vec![]);
    let member_a = make_event(
        "$join_a",
        "m.room.member",
        "@alice:x",
        "@alice:x",
        vec!["$create"],
    );
    let member_b = make_event(
        "$join_b",
        "m.room.member",
        "@alice:x",
        "@alice:x",
        vec!["$create"],
    );

    let mut events_map = std::collections::HashMap::new();
    events_map.insert(create.event_id.clone(), create.clone());
    events_map.insert(member_a.event_id.clone(), member_a.clone());
    events_map.insert(member_b.event_id.clone(), member_b.clone());

    let mut fork_a = imbl::OrdMap::new();
    fork_a.insert(("m.room.create".into(), String::new()), "$create".into());
    fork_a.insert(
        ("m.room.member".into(), "@alice:x".into()),
        "$join_a".into(),
    );

    let mut fork_b = imbl::OrdMap::new();
    fork_b.insert(("m.room.create".into(), String::new()), "$create".into());
    fork_b.insert(
        ("m.room.member".into(), "@alice:x".into()),
        "$join_b".into(),
    );

    let state_maps = vec![fork_a, fork_b];

    let namespaced = namespaced_resolve_state_maps(&state_maps, &events_map, StateResVersion::V2);
    let root = resolve_state_maps(&state_maps, &events_map, StateResVersion::V2);

    assert_eq!(namespaced, root);
}
