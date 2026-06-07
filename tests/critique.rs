use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

fn load_fixture(path: &str) -> Vec<LeanEvent> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("Missing {path}"));
    let val: Value = serde_json::from_str(&content).unwrap();
    if val.is_array() {
        serde_json::from_value(val).unwrap()
    } else {
        serde_json::from_value(val["events"].clone()).unwrap()
    }
}

fn to_event_map(events: &[LeanEvent]) -> HashMap<String, LeanEvent> {
    events
        .iter()
        .map(|e| (e.event_id.clone(), e.clone()))
        .collect()
}

fn assert_expected_matches(input_path: &str, expected_path: &str) {
    let input_events = load_fixture(input_path);
    let map = to_event_map(&input_events);

    let expected_events = load_fixture(expected_path);
    let mut expected_map = HashMap::new();
    for ev in expected_events {
        let key = (ev.event_type.clone(), ev.state_key.clone());
        expected_map.insert(key, ev.event_id.clone());
    }

    let resolved = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2_1_1);

    // Convert resolved state to state keys
    let mut resolved_map = HashMap::new();
    for (key, event_id) in resolved {
        resolved_map.insert(key, event_id);
    }

    // Assert that the keys and values are exactly the same
    assert_eq!(
        resolved_map, expected_map,
        "Mismatch for Anomaly in {}",
        input_path
    );
}

#[test]
fn test_anomaly_01_state_reset() {
    assert_expected_matches(
        "tests/critique_data/01_state_reset.json",
        "tests/critique_expected/01_state_reset.json",
    );
}

#[test]
fn test_anomaly_02_admin_lockout() {
    assert_expected_matches(
        "tests/critique_data/02_admin_lockout.json",
        "tests/critique_expected/02_admin_lockout.json",
    );
}

#[test]
fn test_anomaly_03_phantom_join_rules() {
    assert_expected_matches(
        "tests/critique_data/03_phantom_join_rules.json",
        "tests/critique_expected/03_phantom_join_rules.json",
    );
}

#[test]
fn test_anomaly_04_ban_evasion() {
    assert_expected_matches(
        "tests/critique_data/04_ban_evasion.json",
        "tests/critique_expected/04_ban_evasion.json",
    );
}

#[test]
fn test_anomaly_05_timestamp_spoofing() {
    assert_expected_matches(
        "tests/critique_data/05_timestamp_spoofing.json",
        "tests/critique_expected/05_timestamp_spoofing.json",
    );
}

#[test]
fn test_anomaly_06_action_evaporation() {
    assert_expected_matches(
        "tests/critique_data/06_action_evaporation.json",
        "tests/critique_expected/06_action_evaporation.json",
    );
}

#[test]
fn test_anomaly_06b_mod_membership_evaporation() {
    assert_expected_matches(
        "tests/critique_data/06b_mod_membership_evaporation.json",
        "tests/critique_expected/06b_mod_membership_evaporation.json",
    );
}

#[test]
fn test_anomaly_06c_zombie_invite_reset() {
    assert_expected_matches(
        "tests/critique_data/06c_zombie_invite_reset.json",
        "tests/critique_expected/06c_zombie_invite_reset.json",
    );
}

#[test]
fn test_anomaly_07_state_baseline_pollution() {
    assert_expected_matches(
        "tests/critique_data/07_state_baseline_pollution.json",
        "tests/critique_expected/07_state_baseline_pollution.json",
    );
}

#[test]
fn test_anomaly_08_problem_b() {
    assert_expected_matches(
        "tests/critique_data/08_problem_b.json",
        "tests/critique_expected/08_problem_b.json",
    );
}

#[test]
fn test_anomaly_09_moderator_disappearance() {
    assert_expected_matches(
        "tests/critique_data/09_moderator_disappearance.json",
        "tests/critique_expected/09_moderator_disappearance.json",
    );
}

#[test]
fn test_anomaly_10_vanishing_timelines() {
    assert_expected_matches(
        "tests/critique_data/10_vanishing_timelines.json",
        "tests/critique_expected/10_vanishing_timelines.json",
    );
}

#[test]
fn test_anomaly_11_auth_chain_truncation() {
    assert_expected_matches(
        "tests/critique_data/11_auth_chain_truncation.json",
        "tests/critique_expected/11_auth_chain_truncation.json",
    );
}

#[test]
fn test_anomaly_12_zombie_resurrection() {
    assert_expected_matches(
        "tests/critique_data/12_zombie_resurrection.json",
        "tests/critique_expected/12_zombie_resurrection.json",
    );
}

#[test]
fn test_anomaly_13_large_cascading_lockout() {
    assert_expected_matches(
        "tests/critique_data/13_large_cascading_lockout.json",
        "tests/critique_expected/13_large_cascading_lockout.json",
    );
}

#[test]
fn test_anomaly_14_state_reset_via_redactions() {
    assert_expected_matches(
        "tests/critique_data/14_state_reset_via_redactions.json",
        "tests/critique_expected/14_state_reset_via_redactions.json",
    );
}

#[test]
fn test_anomaly_15_dos_traversal_bfs() {
    assert_expected_matches(
        "tests/critique_data/15_dos_traversal_bfs.json",
        "tests/critique_expected/15_dos_traversal_bfs.json",
    );
}

#[test]
fn test_anomaly_16_causality_leakage() {
    assert_expected_matches(
        "tests/critique_data/16_causality_leakage.json",
        "tests/critique_expected/16_causality_leakage.json",
    );
}
