use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Action {
    name: &'static str,
    event: LeanEvent,
    is_admin: bool,
}

fn create_action(
    id: &str,
    name: &'static str,
    event_type: &str,
    state_key: &str,
    sender: &str,
    content: serde_json::Value,
    is_admin: bool,
) -> Action {
    Action {
        name,
        is_admin,
        event: LeanEvent {
            event_id: id.to_string(),
            event_type: event_type.to_string(),
            state_key: Some(state_key.to_string()),
            sender: sender.to_string(),
            origin_server_ts: 400, // Concurrent timestamp
            content,
            auth_events: vec![
                "$create".to_string(),
                "$pl_base".to_string(),
                "$bob_join".to_string(),
            ],
            ..Default::default()
        },
    }
}

fn main() {
    let create_ev = LeanEvent {
        event_id: "$create".to_string(),
        event_type: "m.room.create".to_string(),
        state_key: Some("".to_string()),
        sender: "@alice:example.com".to_string(),
        origin_server_ts: 100,
        ..Default::default()
    };

    let pl_base = LeanEvent {
        event_id: "$pl_base".to_string(),
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
        auth_events: vec!["$create".to_string(), "$pl_base".to_string()],
        ..Default::default()
    };

    let admin_actions = vec![
        create_action(
            "$alice_ban",
            "Ban Bob",
            "m.room.member",
            "@bob:example.com",
            "@alice:example.com",
            json!({"membership": "ban"}),
            true,
        ),
        create_action(
            "$alice_kick",
            "Kick Bob",
            "m.room.member",
            "@bob:example.com",
            "@alice:example.com",
            json!({"membership": "leave"}),
            true,
        ),
        create_action(
            "$alice_pl",
            "PL Update",
            "m.room.power_levels",
            "",
            "@alice:example.com",
            json!({"users": {"@bob:example.com": 0}, "state_default": 50}),
            true,
        ),
        create_action(
            "$alice_name",
            "Name Change",
            "m.room.name",
            "",
            "@alice:example.com",
            json!({"name": "Alice Room"}),
            true,
        ),
        create_action(
            "$alice_join_rule",
            "Join Rule Invite",
            "m.room.join_rules",
            "",
            "@alice:example.com",
            json!({"join_rule": "invite"}),
            true,
        ),
        create_action(
            "$alice_leave",
            "Alice Leaves",
            "m.room.member",
            "@alice:example.com",
            "@alice:example.com",
            json!({"membership": "leave"}),
            true,
        ),
    ];

    let concurrent_actions = vec![
        create_action(
            "$bob_name",
            "Malicious Name",
            "m.room.name",
            "",
            "@bob:example.com",
            json!({"name": "Hacked"}),
            false,
        ),
        create_action(
            "$bob_join_rule",
            "Malicious Join Rule",
            "m.room.join_rules",
            "",
            "@bob:example.com",
            json!({"join_rule": "public"}),
            false,
        ),
        create_action(
            "$bob_pl",
            "Malicious PL Promo",
            "m.room.power_levels",
            "",
            "@bob:example.com",
            json!({"users": {"@bob:example.com": 100}, "state_default": 50}),
            false,
        ),
        create_action(
            "$bob_leave",
            "Bob Leaves",
            "m.room.member",
            "@bob:example.com",
            "@bob:example.com",
            json!({"membership": "leave"}),
            false,
        ),
        create_action(
            "$bob_rejoin",
            "Bob Rejoins",
            "m.room.member",
            "@bob:example.com",
            "@bob:example.com",
            json!({"membership": "join"}),
            false,
        ),
    ];

    let mut auth_context = HashMap::new();
    auth_context.insert("$create".to_string(), create_ev.clone());
    auth_context.insert("$pl_base".to_string(), pl_base.clone());
    auth_context.insert("$bob_join".to_string(), bob_join.clone());

    let mut unconflicted = BTreeMap::new();
    unconflicted.insert(
        ("m.room.create".to_string(), Some("".to_string())),
        "$create".to_string(),
    );
    unconflicted.insert(
        ("m.room.power_levels".to_string(), Some("".to_string())),
        "$pl_base".to_string(),
    );
    unconflicted.insert(
        (
            "m.room.member".to_string(),
            Some("@bob:example.com".to_string()),
        ),
        "$bob_join".to_string(),
    );

    println!("| Admin Action | Concurrent Action | V2.1 Output | V2.1.1 Output | Status |");
    println!("|---|---|---|---|---|");

    for admin in &admin_actions {
        for concurrent in &concurrent_actions {
            let mut conflicted = HashMap::new();
            conflicted.insert(admin.event.event_id.clone(), admin.event.clone());
            conflicted.insert(concurrent.event.event_id.clone(), concurrent.event.clone());

            let res_21 = resolve_lean(
                unconflicted.clone(),
                conflicted.clone(),
                &auth_context,
                StateResVersion::V2_1,
            );
            let res_211 = resolve_lean(
                unconflicted.clone(),
                conflicted.clone(),
                &auth_context,
                StateResVersion::V2_1_1,
            );

            let key_a = (
                admin.event.event_type.clone(),
                admin.event.state_key.clone(),
            );
            let key_b = (
                concurrent.event.event_type.clone(),
                concurrent.event.state_key.clone(),
            );

            let mut out_21 = String::new();
            let mut out_211 = String::new();

            if let Some(ev) = res_21.get(&key_a) {
                if ev == &admin.event.event_id {
                    out_21.push_str("A ");
                }
            }
            if let Some(ev) = res_21.get(&key_b) {
                if ev == &concurrent.event.event_id {
                    out_21.push('B');
                }
            }

            if let Some(ev) = res_211.get(&key_a) {
                if ev == &admin.event.event_id {
                    out_211.push_str("A ");
                }
            }
            if let Some(ev) = res_211.get(&key_b) {
                if ev == &concurrent.event.event_id {
                    out_211.push('B');
                }
            }

            let out_21 = out_21.trim();
            let out_211 = out_211.trim();

            let status = if out_21 == out_211 {
                "Identical (Non-Inferior)"
            } else if admin.name.contains("Ban") || admin.name.contains("Kick") {
                if out_211 == "A" && (out_21 == "A B" || out_21 == "B") {
                    "**Fixed** (Rejected Malicious Action)"
                } else {
                    "Diverged"
                }
            } else {
                "**REGRESSION**"
            };

            println!(
                "| {} | {} | {} | {} | {} |",
                admin.name, concurrent.name, out_21, out_211, status
            );
        }
    }
}
