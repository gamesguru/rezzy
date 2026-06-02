use ruma_lean::LeanEvent;
use serde_json::json;
use std::fs::File;
use std::io::Write;

fn write_jsonl(filename: &str, events: Vec<LeanEvent>) {
    let mut file = File::create(filename).unwrap();
    for ev in events {
        let serialized = serde_json::to_string(&ev).unwrap();
        writeln!(file, "{}", serialized).unwrap();
    }
    println!("Wrote {}", filename);
}

fn generate_invite_lock() {
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
            "users": {},
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let public_rules = LeanEvent {
        event_id: "$public".to_string(),
        event_type: "m.room.join_rules".to_string(),
        state_key: Some("".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 300,
        content: json!({ "join_rule": "public" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let admin_lock = LeanEvent {
        event_id: "$admin_lock".to_string(),
        event_type: "m.room.join_rules".to_string(),
        state_key: Some("".to_string()),
        sender: "@admin:example.com".to_string(),
        origin_server_ts: 400,
        content: json!({ "join_rule": "invite" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let historical_join = LeanEvent {
        event_id: "$hist_join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@user:example.com".to_string()),
        sender: "@user:example.com".to_string(),
        origin_server_ts: 350,
        content: json!({ "membership": "join" }),
        auth_events: vec![
            "$create".to_string(),
            "$pl".to_string(),
            "$public".to_string(),
        ],
        ..Default::default()
    };

    write_jsonl(
        "docs/invite_lock.jsonl",
        vec![create_ev, pl_ev, public_rules, admin_lock, historical_join],
    );
}

fn generate_ban_evasion() {
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
            "users": { "@bob:example.com": 50 },
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
        content: json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let alice_bans_bob = LeanEvent {
        event_id: "$alice_bans_bob".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@bob:example.com".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 400,
        content: json!({ "membership": "ban" }),
        auth_events: vec!["$create".to_string(), "$pl".to_string()],
        ..Default::default()
    };

    let bob_name_change = LeanEvent {
        event_id: "$bob_name_change".to_string(),
        event_type: "m.room.name".to_string(),
        state_key: Some("".to_string()),
        sender: "@bob:example.com".to_string(),
        origin_server_ts: 500,
        content: json!({ "name": "Hacked by Bob" }),
        auth_events: vec!["$create".to_string(), "$bob_join".to_string()],
        ..Default::default()
    };

    write_jsonl(
        "docs/ban_evasion.jsonl",
        vec![create_ev, pl_ev, bob_join, alice_bans_bob, bob_name_change],
    );
}

fn generate_demotion_evasion() {
    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_promo = LeanEvent {
        event_id: "$pl_promo".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 200,
        content: json!({
            "users": { "@eve:evil.com": 100 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string()],
        ..Default::default()
    };

    let eve_join = LeanEvent {
        event_id: "$eve_join".to_string(),
        event_type: "m.room.member".to_string(),
        state_key: Some("@eve:evil.com".to_string()),
        sender: "@eve:evil.com".to_string(),
        origin_server_ts: 300,
        content: json!({ "membership": "join" }),
        auth_events: vec!["$create".to_string(), "$pl_promo".to_string()],
        ..Default::default()
    };

    let pl_demote = LeanEvent {
        event_id: "$pl_demote".to_string(),
        event_type: "m.room.power_levels".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 400,
        content: json!({
            "users": { "@eve:evil.com": 0 },
            "state_default": 50
        }),
        auth_events: vec!["$create".to_string(), "$pl_promo".to_string()],
        ..Default::default()
    };

    let eve_attack = LeanEvent {
        event_id: "$eve_attack".to_string(),
        event_type: "m.room.name".to_string(),
        state_key: Some("".to_string()),
        sender: "@eve:evil.com".to_string(),
        origin_server_ts: 500,
        content: json!({ "name": "Hacked by Eve" }),
        auth_events: vec!["$create".to_string(), "$eve_join".to_string()],
        ..Default::default()
    };

    write_jsonl(
        "docs/demotion_evasion.jsonl",
        vec![create_ev, pl_promo, eve_join, pl_demote, eve_attack],
    );
}

fn main() {
    generate_invite_lock();
    generate_ban_evasion();
    generate_demotion_evasion();
}
