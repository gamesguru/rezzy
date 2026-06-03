use ruma_lean::{resolve_lean, LeanEvent, StateResVersion};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;

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

#[test]
fn test_all_critique_pathologies() {
    let paths = fs::read_dir("tests/critique_data").expect("Failed to read critique_data dir");
    
    for entry in paths {
        let entry = entry.unwrap();
        let path = entry.path();
        
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        
        let path_str = path.to_str().unwrap();
        println!("Testing critique pathology: {}", path_str);
        
        let events = load_fixture(path_str);
        let map = to_event_map(&events);
        
        // Ensure that resolving these pathologies does not panic for V2.1
        let _resolved_v2_1 = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2_1);
        
        // Ensure that resolving these pathologies does not panic for V2.1.1
        let _resolved_v2_1_1 = resolve_lean(BTreeMap::new(), map.clone(), &map, StateResVersion::V2_1_1);
    }
}
