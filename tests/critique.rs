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

fn assert_no_panic(jsonl_filename: &str) {
    let absolute_path = std::path::Path::new(
        "/home/shane/Documents/school/ou-papers/program-matrix-state-res-v2.1-critique/build/jsonl",
    )
    .join(jsonl_filename);
    let events = load_fixture(&absolute_path);
    let map = to_event_map(&events);

    let versions = [
        StateResVersion::V1,
        StateResVersion::V2,
        StateResVersion::V2_1,
        StateResVersion::V2_1_Synapse,
        StateResVersion::V2_1_Ruma,
        StateResVersion::V2_1_Tuwunel,
        StateResVersion::V2_1_C10y,
        StateResVersion::V2_1_1,
    ];

    for version in versions {
        let _resolved = resolve_lean(BTreeMap::new(), map.clone(), &map, version);
    }
}

#[test]
fn test_anomaly_01_state_reset() {
    assert_no_panic("01_state_reset.jsonl");
}

#[test]
fn test_anomaly_02_admin_lockout() {
    assert_no_panic("02_admin_lockout.jsonl");
}

#[test]
fn test_anomaly_03_phantom_join_rules() {
    assert_no_panic("03_phantom_join_rules.jsonl");
}

#[test]
fn test_anomaly_04_ban_evasion() {
    assert_no_panic("04_ban_evasion.jsonl");
}

#[test]
fn test_anomaly_05_timestamp_spoofing() {
    assert_no_panic("05_timestamp_spoofing.jsonl");
}

#[test]
fn test_anomaly_06_action_evaporation() {
    assert_no_panic("06_action_evaporation.jsonl");
}

#[test]
fn test_anomaly_06b_mod_membership_evaporation() {
    assert_no_panic("06b_mod_membership_evaporation.jsonl");
}

#[test]
fn test_anomaly_06c_zombie_invite_reset() {
    assert_no_panic("06c_zombie_invite_reset.jsonl");
}

#[test]
fn test_anomaly_07_state_baseline_pollution() {
    assert_no_panic("07_state_baseline_pollution.jsonl");
}

#[test]
fn test_anomaly_08_problem_b() {
    assert_no_panic("08_problem_b.jsonl");
}

#[test]
fn test_anomaly_09_moderator_disappearance() {
    assert_no_panic("09_moderator_disappearance.jsonl");
}

#[test]
fn test_anomaly_10_vanishing_timelines() {
    assert_no_panic("10_vanishing_timelines.jsonl");
}

#[test]
fn test_anomaly_11_auth_chain_truncation() {
    assert_no_panic("11_auth_chain_truncation.jsonl");
}

#[test]
fn test_anomaly_12_zombie_resurrection() {
    assert_no_panic("12_zombie_resurrection.jsonl");
}

#[test]
fn test_anomaly_13_large_cascading_lockout() {
    assert_no_panic("13_large_cascading_lockout.jsonl");
}

#[test]
fn test_anomaly_14_state_reset_via_redactions() {
    assert_no_panic("14_state_reset_via_redactions.jsonl");
}

#[test]
fn test_anomaly_15_dos_traversal_bfs() {
    assert_no_panic("15_dos_traversal_bfs.jsonl");
}

#[test]
fn test_anomaly_16_causality_leakage() {
    assert_no_panic("16_causality_leakage.jsonl");
}
