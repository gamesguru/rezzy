use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

fn load_fixture(path: &std::path::Path) -> Vec<LeanEvent> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("Missing {:?}", path));
    if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|_| panic!("Failed to parse line in {:?}", path))
            })
            .collect()
    } else {
        let val: Value = serde_json::from_str(&content).unwrap();
        if val.is_array() {
            serde_json::from_value(val).unwrap()
        } else {
            serde_json::from_value(val["events"].clone()).unwrap()
        }
    }
}

fn to_event_map(events: &[LeanEvent]) -> HashMap<String, LeanEvent> {
    events
        .iter()
        .map(|e| (e.event_id.clone(), e.clone()))
        .collect()
}

fn get_user_power_level(
    resolved: &BTreeMap<(String, Option<String>), String>,
    map: &HashMap<String, LeanEvent>,
    user_id: &str,
) -> i64 {
    let key = ("m.room.power_levels".to_string(), Some("".to_string()));
    if let Some(event_id) = resolved.get(&key) {
        if let Some(ev) = map.get(event_id) {
            if let Some(users) = ev.content.get("users").and_then(|u| u.as_object()) {
                if let Some(pl) = users.get(user_id).and_then(|v| v.as_i64()) {
                    return pl;
                }
            }
        }
    }
    0
}

fn get_membership(
    resolved: &BTreeMap<(String, Option<String>), String>,
    map: &HashMap<String, LeanEvent>,
    user_id: &str,
) -> String {
    let key = ("m.room.member".to_string(), Some(user_id.to_string()));
    if let Some(event_id) = resolved.get(&key) {
        if let Some(ev) = map.get(event_id) {
            if let Some(m) = ev.content.get("membership").and_then(|v| v.as_str()) {
                return m.to_string();
            }
        }
    }
    "none".to_string()
}

fn resolve_pathology(
    jsonl_filename: &str,
) -> (
    BTreeMap<(String, Option<String>), String>,
    HashMap<String, LeanEvent>,
) {
    let absolute_path = std::path::Path::new(
        "/home/shane/Documents/school/ou-papers/program-matrix-state-res-v2.1-critique/build/jsonl",
    )
    .join(jsonl_filename);
    let events = load_fixture(&absolute_path);
    let map = to_event_map(&events);
    let resolved = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2_1_1);
    (resolved, map)
}

#[test]
fn test_anomaly_01_state_reset() {
    let (resolved, map) = resolve_pathology("01_state_reset.jsonl");
    // Under CDO, Alice's power level must remain 100 (state reset prevented)
    assert_eq!(
        get_user_power_level(&resolved, &map, "@alice:example.com"),
        100
    );
}

#[test]
fn test_anomaly_02_admin_lockout() {
    let (resolved, map) = resolve_pathology("02_admin_lockout.jsonl");
    // Under CDO, the spammer's concurrent demotion/lockout of Alice is dropped, Spammer is not joined
    assert_ne!(
        get_membership(&resolved, &map, "@spammer:example.com"),
        "join"
    );
}

#[test]
fn test_anomaly_03_phantom_join_rules() {
    let (resolved, map) = resolve_pathology("03_phantom_join_rules.jsonl");
    // Under CDO, the concurrent join during a lockdown is rejected
    assert_ne!(
        get_membership(&resolved, &map, "@spammer:example.com"),
        "join"
    );
}

#[test]
fn test_anomaly_04_ban_evasion() {
    let (resolved, map) = resolve_pathology("04_ban_evasion.jsonl");
    // Under CDO, Bob's concurrent ban evasion attempt is dropped (he remains not joined)
    assert_ne!(get_membership(&resolved, &map, "@bob:ServerB"), "join");
}

#[test]
fn test_anomaly_05_timestamp_spoofing() {
    let (resolved, map) = resolve_pathology("05_timestamp_spoofing.jsonl");
    // Under CDO, the timestamp-spoofing join is rejected
    assert_ne!(
        get_membership(&resolved, &map, "@spammer:example.com"),
        "join"
    );
}

#[test]
fn test_anomaly_06_action_evaporation() {
    let (resolved, map) = resolve_pathology("06_action_evaporation.jsonl");
    // Under CDO, Spammer's ban is transitively dropped because the join is rejected (no evaporation left behind)
    assert_ne!(
        get_membership(&resolved, &map, "@spammer:example.com"),
        "ban"
    );
}

#[test]
fn test_anomaly_06b_mod_membership_evaporation() {
    let (resolved, map) = resolve_pathology("06b_mod_membership_evaporation.jsonl");
    // Under CDO, Nexy's mod join is dropped, and consequently the ban is dropped transitively
    assert_ne!(get_membership(&resolved, &map, "@nexy:example.com"), "join");
}

#[test]
fn test_anomaly_06c_zombie_invite_reset() {
    let (resolved, map) = resolve_pathology("06c_zombie_invite_reset.jsonl");
    // Under CDO, the zombie invite resurrection is prevented (not invited)
    assert_ne!(
        get_membership(&resolved, &map, "@spammer:example.com"),
        "invite"
    );
}

#[test]
fn test_anomaly_07_state_baseline_pollution() {
    let (resolved, map) = resolve_pathology("07_state_baseline_pollution.jsonl");
    // Under CDO, baseline pollution is dropped, and the spammer is not joined
    assert_ne!(
        get_membership(&resolved, &map, "@spammer:example.com"),
        "join"
    );
}

#[test]
fn test_anomaly_08_problem_b() {
    let (resolved, _map) = resolve_pathology("08_problem_b.jsonl");
    // Under CDO, resolving problem B with cyclic auth chains completes safely
    let pl_key = ("m.room.power_levels".to_string(), Some("".to_string()));
    assert!(resolved.contains_key(&pl_key));
}

#[test]
fn test_anomaly_09_moderator_disappearance() {
    let (resolved, map) = resolve_pathology("09_moderator_disappearance.jsonl");
    // Under CDO, Charlie (the moderator) remains joined (no moderator disappearance)
    assert_eq!(
        get_membership(&resolved, &map, "@charlie:example.com"),
        "join"
    );
}

#[test]
fn test_anomaly_10_vanishing_timelines() {
    let (resolved, _map) = resolve_pathology("10_vanishing_timelines.jsonl");
    // Under CDO, timelines are preserved
    assert!(!resolved.is_empty());
}

#[test]
fn test_anomaly_11_auth_chain_truncation() {
    let (resolved, map) = resolve_pathology("11_auth_chain_truncation.jsonl");
    // Under CDO, truncation attempt fails to authenticate the join
    assert_ne!(
        get_membership(&resolved, &map, "@spammer:example.com"),
        "join"
    );
}

#[test]
fn test_anomaly_12_zombie_resurrection() {
    let (resolved, map) = resolve_pathology("12_zombie_resurrection.jsonl");
    // Under CDO, Alice remains banned/not joined
    assert_ne!(get_membership(&resolved, &map, "@alice:ServerA"), "join");
}

#[test]
fn test_anomaly_13_large_cascading_lockout() {
    let (resolved, map) = resolve_pathology("13_large_cascading_lockout.jsonl");
    // Under CDO, Alice's power level remains 100 (lockout loop dropped)
    assert_eq!(
        get_user_power_level(&resolved, &map, "@alice:example.com"),
        100
    );
}

#[test]
fn test_anomaly_14_state_reset_via_redactions() {
    let (resolved, map) = resolve_pathology("14_state_reset_via_redactions.jsonl");
    // Under CDO, Alice's power level remains 100 (state reset via redaction prevented)
    assert_eq!(
        get_user_power_level(&resolved, &map, "@alice:example.com"),
        100
    );
}

#[test]
fn test_anomaly_15_dos_traversal_bfs() {
    let (resolved, _map) = resolve_pathology("15_dos_traversal_bfs.jsonl");
    // Under CDO, traversal completing without hanging
    assert!(!resolved.is_empty());
}

#[test]
fn test_anomaly_16_causality_leakage() {
    let (resolved, _map) = resolve_pathology("16_causality_leakage.jsonl");
    // Under CDO, causality leakage resolves safely
    assert!(!resolved.is_empty());
}
