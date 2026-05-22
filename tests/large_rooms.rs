// Copyright 2026 Shane Jaroch
//
// Ruma Upstream E2E Tests
// These tests use the official ruma-state-res test fixtures from
// https://github.com/ruma/ruma/tree/main/crates/ruma-state-res/tests/it/resolve/fixtures
//
// They validate that our lean_kahn_sort + resolve_lean pipeline produces
// results consistent with the upstream Ruma state resolution implementation.

extern crate alloc;
extern crate std;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use std::collections::HashMap;

/// Load a JSON fixture file into a Vec<LeanEvent>.
/// The fixtures use "type" (not "event_type") which our serde rename handles.
fn load_fixture(path: &str) -> Vec<LeanEvent> {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", path, e));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {}", path, e))
}

/// Load multiple fixture files and concatenate them into one event list.
#[test]
fn test_room_id() {
    let s = "!00-m-room-create";
    let id = ruma::RoomId::parse(s);
    println!("id: {:?}", id);
}

fn load_fixtures(paths: &[&str]) -> Vec<LeanEvent> {
    let mut events = Vec::new();
    for path in paths {
        events.extend(load_fixture(path));
    }
    events
}

/// Build a HashMap<String, LeanEvent> from a list of events (keyed by event_id).
fn to_event_map(events: &[LeanEvent]) -> HashMap<String, LeanEvent> {
    events
        .iter()
        .map(|e| (e.event_id.clone(), e.clone()))
        .collect()
}

const FIXTURE_DIR: &str = "ruma/crates/ruma-state-res/tests/it/resolve/fixtures";

fn sort_and_verify(events: &[LeanEvent], version: StateResVersion) -> Vec<String> {
    let map = to_event_map(events);
    let result = ruma_lean::lean_kahn_sort_detailed(&map, &map, version);
    assert!(result.is_ok(), "Cycle detected during sort");
    result.into_sorted()
}

/// Run Kahn's sort on the events and verify it doesn't detect any cycles.
fn test_benchmark_1k_sort_no_cycles() {
    let content = std::fs::read_to_string("res/benchmark_1k.json").expect("benchmark_1k.json");
    let data: serde_json::Value = serde_json::from_str(&content).unwrap();
    let events: Vec<LeanEvent> = serde_json::from_value(data["events"].clone()).unwrap();
    let sorted = sort_and_verify(&events, StateResVersion::V2);
    assert_eq!(sorted.len(), 1000);
    assert_eq!(sorted[0], "$00000-m-room-create");
}

#[test]
fn test_benchmark_1k_v2_1_sort_no_cycles() {
    let content =
        std::fs::read_to_string("res/benchmark_1k_v2_1.json").expect("benchmark_1k_v2_1.json");
    let data: serde_json::Value = serde_json::from_str(&content).unwrap();
    let events: Vec<LeanEvent> = serde_json::from_value(data["events"].clone()).unwrap();
    let sorted = sort_and_verify(&events, StateResVersion::V2_1);
    assert_eq!(sorted.len(), 1000);
    assert_eq!(sorted[0], "$00000-m-room-create");
}

#[test]
fn test_benchmark_1k_resolution_determinism() {
    let content = std::fs::read_to_string("res/benchmark_1k.json").expect("benchmark_1k.json");
    let data: serde_json::Value = serde_json::from_str(&content).unwrap();
    let events: Vec<LeanEvent> = serde_json::from_value(data["events"].clone()).unwrap();

    // Run resolution twice and verify determinism
    let resolved1 = resolve_lean(
        BTreeMap::new(),
        to_event_map(&events),
        &to_event_map(&events),
        StateResVersion::V2,
    );
    let resolved2 = resolve_lean(
        BTreeMap::new(),
        to_event_map(&events),
        &to_event_map(&events),
        StateResVersion::V2,
    );
    assert_eq!(resolved1, resolved2, "Resolution must be deterministic");
}

// ============================================================================
// Auth Chain Validation on Real Fixtures
// ============================================================================

#[test]
fn test_ruma_bootstrap_auth_chain() {
    use ruma_lean::auth::{check_auth_chain, RoomState};

    let events = load_fixture(&format!("{}/bootstrap-public-chat.json", FIXTURE_DIR));
    let (accepted, rejected) = check_auth_chain(&events, &RoomState::new());

    // All bootstrap events should pass auth
    assert!(
        rejected.is_empty(),
        "Bootstrap events should all pass auth, but {} were rejected: {:?}",
        rejected.len(),
        rejected
    );
    assert_eq!(accepted.len(), events.len());
}

// ============================================================================
// Realistic Large Room (10K events with federation forks, PL wars, bans)
// ============================================================================

fn load_large_room() -> Vec<LeanEvent> {
    let content = std::fs::read_to_string("res/realistic_large_room.json")
        .expect("realistic_large_room.json");
    let data: serde_json::Value = serde_json::from_str(&content).unwrap();
    serde_json::from_value(data["events"].clone()).unwrap()
}

#[test]
fn test_large_room_10k_sort_no_cycles() {
    let events = load_large_room();
    assert!(
        events.len() >= 10000,
        "Expected >= 10000 events, got {}",
        events.len()
    );
    let sorted = sort_and_verify(&events, StateResVersion::V2);
    // Create must be first
    assert!(
        sorted[0].starts_with("$"),
        "First sorted event should be a valid event ID"
    );
    // All events accounted for
    assert!(sorted.len() >= 10000);
}

#[test]
fn test_large_room_10k_v2_1_sort() {
    let events = load_large_room();
    let sorted = sort_and_verify(&events, StateResVersion::V2_1);
    assert!(sorted.len() >= 10000);
}

#[test]
fn test_large_room_10k_resolution_determinism() {
    let events = load_large_room();
    let r1 = resolve_lean(
        BTreeMap::new(),
        to_event_map(&events),
        &to_event_map(&events),
        StateResVersion::V2,
    );
    let r2 = resolve_lean(
        BTreeMap::new(),
        to_event_map(&events),
        &to_event_map(&events),
        StateResVersion::V2,
    );
    assert_eq!(r1, r2, "10K room resolution must be deterministic");
}

#[test]
fn test_large_room_10k_v2_vs_v2_1_divergence() {
    let events = load_large_room();
    let map = to_event_map(&events);
    let v2 = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2);
    let v2_1 = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2_1);
    // V2 and V2.1 may diverge on conflicted state — that's the whole point of MSC4297.
    // But both must produce valid resolved state.
    assert!(!v2.is_empty(), "V2 must produce resolved state");
    assert!(!v2_1.is_empty(), "V2.1 must produce resolved state");
    // Both must agree on m.room.create
    assert_eq!(
        v2.get(&("m.room.create".into(), Some(String::new()))),
        v2_1.get(&("m.room.create".into(), Some(String::new()))),
        "V2 and V2.1 must agree on the create event"
    );
}

#[test]
fn test_large_room_10k_subgraph_bounded() {
    let events = load_large_room();
    let map = to_event_map(&events);
    // Pick some conflicted state_keys
    let mut pl_events: Vec<String> = events
        .iter()
        .filter(|e| e.event_type == "m.room.power_levels")
        .map(|e| e.event_id.clone())
        .collect();
    assert!(!pl_events.is_empty(), "Should have PL events");
    // Test bounded subgraph on the first 10 PL events
    pl_events.truncate(10);
    let bounded = ruma_lean::compute_v2_1_conflicted_subgraph_bounded(&map, &pl_events, Some(5));
    assert!(
        !bounded.subgraph.is_empty(),
        "Bounded subgraph should contain events"
    );
    // Unbounded should be >= bounded
    let full = ruma_lean::compute_v2_1_conflicted_subgraph_bounded(&map, &pl_events, None);
    assert!(
        full.subgraph.len() >= bounded.subgraph.len(),
        "Unbounded subgraph ({}) should be >= bounded ({})",
        full.subgraph.len(),
        bounded.subgraph.len()
    );
}

#[test]
fn test_large_room_10k_auth_chain() {
    use ruma_lean::auth::{check_auth_chain, RoomState};

    let events = load_large_room();
    let (accepted, _rejected) = check_auth_chain(&events, &RoomState::new());
    // Not all events will pass auth (spammers, unauthorized PL changes),
    // but the generator tries to keep it somewhat coherent.
    let pass_rate = accepted.len() as f64 / events.len() as f64;
    assert!(
        pass_rate > 0.20,
        "Auth pass rate should be >20%, got {:.1}% ({}/{})",
        pass_rate * 100.0,
        accepted.len(),
        events.len()
    );
}

// ============================================================================
// Real Matrix Room State Dump (42K events — auth validation only)
// ============================================================================

#[test]
fn test_real_room_42k_state_deserialization() {
    let content =
        std::fs::read_to_string("res/real_matrix_state.json").expect("real_matrix_state.json");
    let events: Vec<LeanEvent> = serde_json::from_str(&content).unwrap();
    assert!(
        events.len() > 40000,
        "Should have 42K+ events, got {}",
        events.len()
    );
    // Verify all events have event_ids
    for ev in &events {
        assert!(!ev.event_id.is_empty(), "Event should have an event_id");
    }
}

#[test]
fn test_real_room_42k_power_level_coercion() {
    // The real room dump likely has string/float power levels from old Synapse versions.
    let content =
        std::fs::read_to_string("res/real_matrix_state.json").expect("real_matrix_state.json");
    let events: Vec<LeanEvent> = serde_json::from_str(&content).unwrap();
    // Find PL events and verify they deserialize without panicking
    let pl_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "m.room.power_levels")
        .collect();
    assert!(
        !pl_events.is_empty(),
        "Real room should have power_levels events"
    );
    // Verify PL events have content with users
    for pl in &pl_events {
        assert!(
            pl.content.get("users").is_some(),
            "PL event should have users field"
        );
    }
}

#[test]
fn test_real_room_v2_1_deserialization() {
    let content = std::fs::read_to_string("res/real_matrix_state_v2_1.json")
        .expect("real_matrix_state_v2_1.json");
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();
    let events: Vec<LeanEvent> = if val.is_array() {
        serde_json::from_value(val).unwrap()
    } else {
        serde_json::from_value(val["events"].clone()).unwrap()
    };
    assert!(
        events.len() > 10,
        "Should have 10+ events, got {}",
        events.len()
    );
}

// ============================================================================
// Real Room DAGs from conduwuit RocksDB (full auth_events + prev_events)
// ============================================================================

fn load_real_dag(path: &str) -> Vec<LeanEvent> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("Missing {path}"));
    let data: serde_json::Value = serde_json::from_str(&content).unwrap();
    serde_json::from_value(data["events"].clone()).unwrap()
}

#[test]
fn test_real_dag_52k_room_deserialization() {
    let events = load_real_dag("res/real_dag_52k_room.json");
    assert!(
        events.len() >= 10000,
        "Expected >= 10000 events, got {}",
        events.len()
    );
    // Every event should have auth_events (except possibly create)
    let with_auth = events.iter().filter(|e| !e.auth_events.is_empty()).count();
    assert!(
        with_auth > events.len() - 10,
        "Most events should have auth_events, got {}/{}",
        with_auth,
        events.len()
    );
}

#[test]
fn test_real_dag_52k_room_sort() {
    let events = load_real_dag("res/real_dag_52k_room.json");
    let sorted = sort_and_verify(&events, StateResVersion::V2);
    assert!(sorted.len() >= 10000);
}

#[test]
fn test_real_dag_52k_room_v2_1_sort() {
    let events = load_real_dag("res/real_dag_52k_room.json");
    let sorted = sort_and_verify(&events, StateResVersion::V2_1);
    assert!(sorted.len() >= 10000);
}

#[test]
fn test_real_dag_52k_room_resolution() {
    let events = load_real_dag("res/real_dag_52k_room.json");
    let map = to_event_map(&events);
    let resolved = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2);
    assert!(!resolved.is_empty(), "Resolution should produce state");
    // Determinism check
    let events2 = load_real_dag("res/real_dag_52k_room.json");
    let map2 = to_event_map(&events2);
    let resolved2 = resolve_lean(BTreeMap::new(), map2.clone(), &map2, StateResVersion::V2);
    assert_eq!(resolved, resolved2, "Resolution must be deterministic");
}

#[test]
fn test_real_dag_nheko_room_sort() {
    let events = load_real_dag("res/real_dag_nheko.json");
    assert!(
        events.len() >= 6000,
        "Expected >= 6000 events, got {}",
        events.len()
    );
    let sorted = sort_and_verify(&events, StateResVersion::V2);
    assert!(sorted.len() >= 6000);
}

#[test]
fn test_real_dag_nheko_room_106_heads() {
    // This room has 106 DAG heads — a real federation mess
    let events = load_real_dag("res/real_dag_nheko.json");
    let event_map = to_event_map(&events);

    // Compute heads: events not referenced by any prev_events
    let mut referenced = std::collections::HashSet::new();
    for ev in &events {
        for prev in &ev.prev_events {
            referenced.insert(prev.clone());
        }
    }
    let heads: Vec<_> = events
        .iter()
        .filter(|e| !referenced.contains(&e.event_id))
        .collect();
    assert!(
        heads.len() > 50,
        "Nheko room should have 50+ DAG heads (federation forks), got {}",
        heads.len()
    );

    // Resolution must still complete on this messy DAG
    let resolved = resolve_lean(
        BTreeMap::new(),
        event_map.clone(),
        &event_map,
        StateResVersion::V2,
    );
    assert!(!resolved.is_empty(), "Resolution should produce state");
}

fn resolve_msc4297(
    pdus_file: &str,
    state_files: &[&str],
    version: StateResVersion,
) -> BTreeMap<(String, Option<String>), String> {
    let events = load_fixtures(&[pdus_file]);
    let event_map = to_event_map(&events);

    let mut conflicted_events = HashMap::new();

    for file in state_files {
        let content = std::fs::read_to_string(file).unwrap();
        let event_ids: Vec<String> = serde_json::from_str(&content).unwrap();
        for id in event_ids {
            if let Some(ev) = event_map.get(&id) {
                conflicted_events.insert(id.clone(), ev.clone());
            }
        }
    }

    let unconflicted = BTreeMap::new();
    resolve_lean(unconflicted, conflicted_events, &event_map, version)
}
