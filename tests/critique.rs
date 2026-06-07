use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

type ResolvedStateMap = BTreeMap<(String, Option<String>), String>;
type EventMap = HashMap<String, LeanEvent>;

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

fn to_event_map(events: &[LeanEvent]) -> EventMap {
    events
        .iter()
        .map(|e| (e.event_id.clone(), e.clone()))
        .collect()
}

fn get_user_power_level(resolved: &ResolvedStateMap, map: &EventMap, user_id: &str) -> i64 {
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

fn get_membership(resolved: &ResolvedStateMap, map: &EventMap, user_id: &str) -> String {
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

fn resolve_pathology(jsonl_filename: &str) -> (ResolvedStateMap, EventMap) {
    let absolute_path = std::path::Path::new(
        "/home/shane/Documents/school/ou-papers/program-matrix-state-res-v2.1-critique/build/jsonl",
    )
    .join(jsonl_filename);
    let events = load_fixture(&absolute_path);
    let map = to_event_map(&events);
    let resolved = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2_1_1);
    (resolved, map)
}

fn assert_benign_convergence(jsonl_filename: &str) -> (ResolvedStateMap, EventMap) {
    let absolute_path = std::path::Path::new(
        "/home/shane/Documents/school/ou-papers/program-matrix-state-res-v2.1-critique/build/jsonl",
    )
    .join(jsonl_filename);
    let events = load_fixture(&absolute_path);
    let map = to_event_map(&events);

    let resolved_v2_1 = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2_1);
    let resolved_v2_1_1 = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2_1_1);

    assert_eq!(
        resolved_v2_1_1, resolved_v2_1,
        "Causal Domination pre-filter violated Benign Convergence parity for {}",
        jsonl_filename
    );
    (resolved_v2_1_1, map)
}

#[test]
fn test_anomaly_01_state_reset() {
    let (resolved, map) = assert_benign_convergence("01_state_reset.jsonl");
    // Assert specific state values: Alice and Bob are joined, Bob has power level 0
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(get_user_power_level(&resolved, &map, "@bob:example.com"), 0);
}

#[test]
fn test_anomaly_02_admin_lockout() {
    let (resolved, map) = assert_benign_convergence("02_admin_lockout.jsonl");
    // Assert Alice is joined and remains secure, Bob has power level 0
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(get_user_power_level(&resolved, &map, "@bob:example.com"), 0);
}

#[test]
fn test_anomaly_03_phantom_join_rules() {
    let (resolved, map) = resolve_pathology("03_phantom_join_rules.jsonl");
    // Under CDO, Charlie's concurrent join during lockdown is dropped
    assert_ne!(
        get_membership(&resolved, &map, "@charlie:example.com"),
        "join"
    );
    // Assert Alice and Charlie's baseline states
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
}

#[test]
fn test_anomaly_04_ban_evasion() {
    let (resolved, map) = resolve_pathology("04_ban_evasion.jsonl");
    // Under CDO, Bob's concurrent ban evasion is dropped, Bob remains banned (membership "ban")
    assert_eq!(get_membership(&resolved, &map, "@bob:ServerB"), "ban");
    assert_eq!(get_membership(&resolved, &map, "@alice:ServerA"), "join");
}

#[test]
fn test_anomaly_05_timestamp_spoofing() {
    let (resolved, map) = assert_benign_convergence("05_timestamp_spoofing.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(
        get_user_power_level(&resolved, &map, "@bob:example.com"),
        50
    );
}

#[test]
fn test_anomaly_06_action_evaporation() {
    let (resolved, map) = assert_benign_convergence("06_action_evaporation.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(get_user_power_level(&resolved, &map, "@bob:example.com"), 0);
}

#[test]
fn test_anomaly_06b_mod_membership_evaporation() {
    let (resolved, map) = resolve_pathology("06b_mod_membership_evaporation.jsonl");
    // Under CDO, Nexy's mod join was dropped, so Nexy is not joined
    assert_ne!(get_membership(&resolved, &map, "@nexy:example.com"), "join");
}

#[test]
fn test_anomaly_06c_zombie_invite_reset() {
    let (resolved, map) = assert_benign_convergence("06c_zombie_invite_reset.jsonl");
    // Verifies Spammer is banned and Nexy remains joined
    assert_eq!(
        get_membership(&resolved, &map, "@admin:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@nexy:example.com"), "join");
    assert_eq!(
        get_membership(&resolved, &map, "@spammer:example.com"),
        "ban"
    );
}

#[test]
fn test_anomaly_07_state_baseline_pollution() {
    let (resolved, map) = assert_benign_convergence("07_state_baseline_pollution.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "leave"
    );
}

#[test]
fn test_anomaly_08_problem_b() {
    let (resolved, map) = assert_benign_convergence("08_problem_b.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(
        get_user_power_level(&resolved, &map, "@alice:example.com"),
        100
    );
    assert_eq!(
        get_user_power_level(&resolved, &map, "@bob:example.com"),
        50
    );
}

#[test]
fn test_anomaly_09_moderator_disappearance() {
    let (resolved, map) = assert_benign_convergence("09_moderator_disappearance.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(
        get_membership(&resolved, &map, "@charlie:example.com"),
        "ban"
    );
}

#[test]
fn test_anomaly_10_vanishing_timelines() {
    let (resolved, map) = assert_benign_convergence("10_vanishing_timelines.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(
        get_user_power_level(&resolved, &map, "@alice:example.com"),
        100
    );
}

#[test]
fn test_anomaly_11_auth_chain_truncation() {
    let (resolved, map) = assert_benign_convergence("11_auth_chain_truncation.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(
        get_user_power_level(&resolved, &map, "@alice:example.com"),
        100
    );
    assert_eq!(
        get_user_power_level(&resolved, &map, "@bob:example.com"),
        50
    );
}

#[test]
fn test_anomaly_12_zombie_resurrection() {
    let (resolved, map) = assert_benign_convergence("12_zombie_resurrection.jsonl");
    assert_eq!(get_membership(&resolved, &map, "@alice:ServerA"), "ban");
    assert_eq!(get_membership(&resolved, &map, "@bob:ServerB"), "join");
    assert_eq!(get_membership(&resolved, &map, "@charlie:ServerA"), "join");
}

#[test]
fn test_anomaly_13_large_cascading_lockout() {
    let (resolved, map) = resolve_pathology("13_large_cascading_lockout.jsonl");
    // Under CDO, Grace is not banned, Bob and Charlie keep their administrative levels
    assert_ne!(get_membership(&resolved, &map, "@grace:example.com"), "ban");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
}

#[test]
fn test_anomaly_14_state_reset_via_redactions() {
    let (resolved, map) = assert_benign_convergence("14_state_reset_via_redactions.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
}

#[test]
fn test_anomaly_15_dos_traversal_bfs() {
    let (resolved, map) = assert_benign_convergence("15_dos_traversal_bfs.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "join"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(
        get_user_power_level(&resolved, &map, "@bob:example.com"),
        50
    );
}

#[test]
fn test_anomaly_16_causality_leakage() {
    let (resolved, map) = assert_benign_convergence("16_causality_leakage.jsonl");
    assert_eq!(
        get_membership(&resolved, &map, "@alice:example.com"),
        "leave"
    );
    assert_eq!(get_membership(&resolved, &map, "@bob:example.com"), "join");
    assert_eq!(
        get_user_power_level(&resolved, &map, "@bob:example.com"),
        100
    );
}
