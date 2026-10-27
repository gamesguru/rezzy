//! State resolution v2 power-event classification.
//!
//! The spec defines a power event as `m.room.power_levels`, `m.room.join_rules`,
//! or an `m.room.member` with membership `leave` or `ban` whose `sender` is not
//! its `state_key` (a kick or ban). The definition is the same for v2 and v2.1:
//! a user's own join or profile change is not a power event, so it is ordered
//! by the mainline ordering (mainline position, `origin_server_ts`, `event_id`)
//! rather than by the reverse topological power ordering.

use crate::utils;
use crate::utils::to_event_map;

use rezzy::basespec::event_types::EventType;
use rezzy::resolve::multi::resolve_state_maps_lazy_with_diff;
use rezzy::resolve::semilattice::resolve_semilattice_fold;
use rezzy::{LeanEvent, PersistentOrdMap, StateResVersion};
use std::collections::HashMap;

const ALICE: &str = "@alice:a.com";
const BOB: &str = "@bob:b.com";

const ROOM: &str = r#"
{"event_id":"$create","type":"m.room.create","state_key":"","sender":"@alice:a.com","depth":0,"origin_server_ts":1000,"content":{"creator":"@alice:a.com","room_version":"10"},"prev_events":[],"auth_events":[]}
{"event_id":"$alice_join","type":"m.room.member","state_key":"@alice:a.com","sender":"@alice:a.com","depth":1,"origin_server_ts":1001,"content":{"membership":"join"},"prev_events":["$create"],"auth_events":["$create"]}
{"event_id":"$pl","type":"m.room.power_levels","state_key":"","sender":"@alice:a.com","depth":2,"origin_server_ts":1002,"content":{"users":{"@alice:a.com":100},"events_default":0,"state_default":50,"ban":50,"kick":50,"invite":0},"prev_events":["$alice_join"],"auth_events":["$create","$alice_join"]}
{"event_id":"$jr","type":"m.room.join_rules","state_key":"","sender":"@alice:a.com","depth":3,"origin_server_ts":1003,"content":{"join_rule":"public"},"prev_events":["$pl"],"auth_events":["$create","$alice_join","$pl"]}
{"event_id":"$bob_join","type":"m.room.member","state_key":"@bob:b.com","sender":"@bob:b.com","depth":4,"origin_server_ts":1010,"content":{"membership":"join"},"prev_events":["$jr"],"auth_events":["$create","$pl","$jr"]}
"#;

/// Bob's profile changes A -> B -> C -> D -> E, each authed by the one before.
/// B has the lexicographically largest event ID, as in the Complement run
/// where Synapse's Python resolver and gomatrixserverlib both picked B.
const CHAIN: [(&str, &str); 5] = [
    ("A", "$m_a"),
    ("B", "$m_z"),
    ("C", "$m_c"),
    ("D", "$m_d"),
    ("E", "$m_e"),
];

fn member(
    id: &str,
    sender: &str,
    state_key: &str,
    content: &str,
    ts: u64,
    auth: &[&str],
) -> LeanEvent {
    let auth = auth
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(",");
    let line = format!(
        r#"{{"event_id":"{id}","type":"m.room.member","state_key":"{state_key}","sender":"{sender}","depth":5,"origin_server_ts":{ts},"content":{content},"prev_events":["$jr"],"auth_events":[{auth}]}}"#
    );
    utils::parse_jsonl_events(&line).remove(0)
}

fn room_with_chain(tied: bool) -> HashMap<String, LeanEvent> {
    let mut events = utils::parse_jsonl_events(ROOM);
    let mut prev = "$bob_join";
    for ((name, id), distinct_ts) in CHAIN.iter().zip(2000_u64..) {
        let ts = if tied { 2000 } else { distinct_ts };
        let content = format!(r#"{{"membership":"join","displayname":"{name}"}}"#);
        events.push(member(
            id,
            BOB,
            BOB,
            &content,
            ts,
            &["$create", "$pl", "$jr", prev],
        ));
        prev = id;
    }
    to_event_map(&events)
}

fn bob_key() -> (EventType, String) {
    (EventType::from("m.room.member"), BOB.to_string())
}

/// Our state still has Bob's original join; theirs has E at the end of the chain.
fn fork_states() -> Vec<PersistentOrdMap<(EventType, String), String>> {
    let mut common = PersistentOrdMap::new();
    common.insert(
        (EventType::from("m.room.create"), String::new()),
        "$create".to_string(),
    );
    common.insert(
        (EventType::from("m.room.member"), ALICE.to_string()),
        "$alice_join".to_string(),
    );
    common.insert(
        (EventType::from("m.room.power_levels"), String::new()),
        "$pl".to_string(),
    );
    common.insert(
        (EventType::from("m.room.join_rules"), String::new()),
        "$jr".to_string(),
    );
    let mut ours = common.clone();
    ours.insert(bob_key(), "$bob_join".to_string());
    let mut theirs = common;
    theirs.insert(bob_key(), "$m_e".to_string());
    vec![ours, theirs]
}

fn name_of(event_id: &str) -> &str {
    CHAIN
        .iter()
        .find(|(_, id)| *id == event_id)
        .map_or(event_id, |(name, _)| name)
}

// ============================================================================
// is_power_event
// ============================================================================

fn assert_power_event_classification(version: StateResVersion) {
    let auth = ["$create", "$pl", "$jr"];
    let cases = [
        (
            "own join",
            member("$j", BOB, BOB, r#"{"membership":"join"}"#, 1, &auth),
            false,
        ),
        (
            "own profile change",
            member(
                "$p",
                BOB,
                BOB,
                r#"{"membership":"join","displayname":"Bob"}"#,
                1,
                &auth,
            ),
            false,
        ),
        (
            "own leave",
            member("$l", BOB, BOB, r#"{"membership":"leave"}"#, 1, &auth),
            false,
        ),
        (
            "invite",
            member("$i", ALICE, BOB, r#"{"membership":"invite"}"#, 1, &auth),
            false,
        ),
        (
            "kick",
            member("$k", ALICE, BOB, r#"{"membership":"leave"}"#, 1, &auth),
            true,
        ),
        (
            "ban",
            member("$b", ALICE, BOB, r#"{"membership":"ban"}"#, 1, &auth),
            true,
        ),
    ];
    for (name, ev, want) in cases {
        let want = if version == StateResVersion::V1
            && ev.event_type == rezzy::basespec::event_types::M_ROOM_MEMBER
        {
            true
        } else {
            want
        };
        assert_eq!(ev.is_power_event(version), want, "{name} under {version:?}");
    }
}

#[test]
fn test_v2_membership_power_events_follow_spec() {
    assert_power_event_classification(StateResVersion::V2);
}

#[test]
fn test_v2_1_membership_power_events_follow_spec() {
    assert_power_event_classification(StateResVersion::V2_1);
}

// ============================================================================
// End-to-end: A-E profile-change chain (Complement
// TestStateIdsFallbackFetchesFullAuthChain, room version 10)
// ============================================================================

/// With distinct timestamps, E is last in mainline order under any reading.
#[test]
fn test_v2_profile_chain_distinct_timestamps_resolves_to_last() {
    let map = room_with_chain(false);
    let resolved = rezzy::resolve_state_maps(&fork_states(), &map, StateResVersion::V2);
    assert_eq!(name_of(&resolved[&bob_key()]), "E");
}

/// A-E are in the auth difference, so they join the full conflicted set. As
/// non-power events with one mainline position and one timestamp, `event_id`
/// orders them, and the largest (B) is applied last and wins. Synapse's Python
/// v2 resolver and gomatrixserverlib both return B here.
#[test]
fn test_v2_profile_chain_tied_timestamps_orders_by_event_id() {
    let map = room_with_chain(true);
    let resolved = rezzy::resolve_state_maps(&fork_states(), &map, StateResVersion::V2);
    assert_eq!(resolved.get(&bob_key()).map(|id| name_of(id)), Some("B"));
}

/// The same fork through the lattice fold, given the full conflicted set as
/// Synapse passes it (`resolve_v2_via_lattice_fold`).
#[test]
fn test_v2_lattice_fold_profile_chain_tied_timestamps_orders_by_event_id() {
    let map = room_with_chain(true);
    let mut unconflicted = fork_states().remove(0);
    unconflicted.remove(&bob_key());
    let mut conflicted = HashMap::new();
    for id in ["$bob_join", "$m_a", "$m_z", "$m_c", "$m_d", "$m_e"] {
        conflicted.insert(id.to_string(), map[id].clone());
    }
    let resolved = resolve_semilattice_fold(&unconflicted, &conflicted, &map, StateResVersion::V2);
    assert_eq!(name_of(&resolved[&bob_key()]), "B");
}

/// v2 and v2.1 share the power-event definition and the mainline ordering, and
/// both put A-E in the full conflicted set, so they agree on this fork.
#[test]
fn test_v2_and_v2_1_agree_on_tied_profile_chain() {
    let map = room_with_chain(true);
    let v2 = rezzy::resolve_state_maps(&fork_states(), &map, StateResVersion::V2);
    let v2_1 = rezzy::resolve_state_maps(&fork_states(), &map, StateResVersion::V2_1);
    assert_eq!(name_of(&v2[&bob_key()]), name_of(&v2_1[&bob_key()]));
}

#[test]
fn test_v2_lazy_profile_chain_tied_timestamps_orders_by_event_id_without_diff() {
    let map = room_with_chain(true);
    let resolved = resolve_state_maps_lazy_with_diff(
        &fork_states(),
        &map,
        None::<Vec<String>>,
        StateResVersion::V2,
    );
    assert_eq!(name_of(&resolved[&bob_key()]), "B");
}

#[test]
fn test_v2_lazy_profile_chain_tied_timestamps_orders_by_event_id_with_diff() {
    let map = room_with_chain(true);
    let resolved = resolve_state_maps_lazy_with_diff(
        &fork_states(),
        &map,
        Some(
            [
                vec!["$bob_join".to_string()],
                CHAIN.iter().map(|(_, id)| (*id).to_string()).collect(),
            ]
            .concat(),
        ),
        StateResVersion::V2,
    );
    assert_eq!(resolved.get(&bob_key()).map(|id| name_of(id)), Some("B"));
}

/// Auth-difference events take part in resolution, so a power-levels event in
/// the difference is sorted and applied. The key it names is agreed by every
/// fork, so the resolved state must still carry the agreed event, and the
/// disputed membership must still resolve as before.
#[test]
fn test_v2_auth_difference_power_levels_does_not_overwrite_agreed_key() {
    let mut events = room_with_chain(true);
    events.insert(
        "$pl_diff".into(),
        utils::parse_jsonl_events(
            r#"{"event_id":"$pl_diff","type":"m.room.power_levels","state_key":"","sender":"@alice:a.com","depth":3,"origin_server_ts":1500,"content":{"users":{"@alice:a.com":100,"@bob:b.com":100},"events_default":0,"state_default":0,"ban":50,"kick":50,"invite":0},"prev_events":["$pl"],"auth_events":["$create","$alice_join","$pl"]}"#,
        )
        .remove(0),
    );
    // Only A cites it, so it is in one fork's chain and not the other's: it is
    // in the auth difference, while the power-levels slot itself is agreed.
    events
        .get_mut("$m_a")
        .unwrap()
        .auth_events
        .push("$pl_diff".into());

    let resolved = rezzy::resolve_state_maps(&fork_states(), &events, StateResVersion::V2);
    let pl_key = (EventType::from("m.room.power_levels"), String::new());
    assert_eq!(resolved.get(&pl_key), Some(&"$pl".to_string()));
    assert_eq!(resolved.get(&bob_key()).map(|id| name_of(id)), Some("B"));
}

/// V1 resolves `m.room.member` conflicts in a dedicated pass (spec rooms/v1),
/// so every membership is a power event there, unlike v2 and later.
#[test]
fn test_v1_all_memberships_are_power_events() {
    let auth = ["$create", "$pl", "$jr"];
    for (name, ev) in [
        (
            "own join",
            member("$j", BOB, BOB, r#"{"membership":"join"}"#, 1, &auth),
        ),
        (
            "own leave",
            member("$l", BOB, BOB, r#"{"membership":"leave"}"#, 1, &auth),
        ),
        (
            "invite",
            member("$i", ALICE, BOB, r#"{"membership":"invite"}"#, 1, &auth),
        ),
    ] {
        assert!(ev.is_power_event(StateResVersion::V1), "{name} under V1");
        assert!(!ev.is_power_event(StateResVersion::V2), "{name} under V2");
    }
}

/// An ancestor cited by the conflicted events of *every* fork is in the
/// intersection of the forks' auth chains, so the spec leaves it out of the
/// auth difference even when no unconflicted event cites it.
///
/// `$x` (and Bob's original join) are cited by both tips but by nothing
/// unconflicted. `$x` is a later-timestamped event for the same key: if it were
/// wrongly treated as part of the auth difference it would be applied last and
/// win. The tips tie on timestamp, so `event_id` decides and `$f2` wins.
fn shared_ancestor_room() -> HashMap<String, LeanEvent> {
    let mut events = utils::parse_jsonl_events(ROOM);
    let auth = ["$create", "$pl", "$jr", "$bob_join"];
    let mut x = member(
        "$x",
        BOB,
        BOB,
        r#"{"membership":"join","displayname":"X"}"#,
        4000,
        &auth,
    );
    x.depth = 5;
    events.push(x);
    for id in ["$f1", "$f2"] {
        let mut tip = member(
            id,
            BOB,
            BOB,
            r#"{"membership":"join","displayname":"tip"}"#,
            3000,
            &["$create", "$pl", "$jr", "$x"],
        );
        tip.depth = 6;
        events.push(tip);
    }
    to_event_map(&events)
}

fn shared_ancestor_forks() -> Vec<PersistentOrdMap<(EventType, String), String>> {
    let mut states = fork_states();
    states[0].insert(bob_key(), "$f1".to_string());
    states[1].insert(bob_key(), "$f2".to_string());
    states
}

#[test]
fn test_v2_shared_auth_ancestor_is_not_in_auth_difference() {
    let map = shared_ancestor_room();
    let resolved = rezzy::resolve_state_maps(&shared_ancestor_forks(), &map, StateResVersion::V2);
    assert_eq!(resolved.get(&bob_key()).map(String::as_str), Some("$f2"));
}

#[test]
fn test_v2_lazy_shared_auth_ancestor_is_not_in_auth_difference() {
    let map = shared_ancestor_room();
    let resolved = resolve_state_maps_lazy_with_diff(
        &shared_ancestor_forks(),
        &map,
        None::<Vec<String>>,
        StateResVersion::V2,
    );
    assert_eq!(resolved.get(&bob_key()).map(String::as_str), Some("$f2"));
}

/// An auth-difference power event is applied to the partially resolved state
/// like any other power event (spec v2 step 2; Synapse's
/// `_iterative_auth_checks`), so it changes how later events are authorized,
/// even though its own key is restored from the unconflicted state at the end.
///
/// Only Bob's room name cites `$pl_diff`, which lowers `state_default` to 0, so
/// `$pl_diff` is in the auth difference. Applied, it authorizes Bob's name,
/// whose mainline position is then later than Alice's, so Bob's name wins.
const AUTH_DIFF_POWER_EVENT_BRANCH: &str = r#"
{"event_id":"$pl_diff","type":"m.room.power_levels","state_key":"","sender":"@alice:a.com","depth":5,"origin_server_ts":1500,"content":{"users":{"@alice:a.com":100},"events_default":0,"state_default":0,"ban":50,"kick":50,"invite":0},"prev_events":["$bob_join"],"auth_events":["$create","$alice_join","$pl"]}
{"event_id":"$name_alice","type":"m.room.name","state_key":"","sender":"@alice:a.com","depth":5,"origin_server_ts":1600,"content":{"name":"alice"},"prev_events":["$bob_join"],"auth_events":["$create","$alice_join","$pl"]}
{"event_id":"$name_bob","type":"m.room.name","state_key":"","sender":"@bob:b.com","depth":6,"origin_server_ts":1700,"content":{"name":"bob"},"prev_events":["$pl_diff"],"auth_events":["$create","$bob_join","$pl_diff"]}
"#;

type StateMap = PersistentOrdMap<(EventType, String), String>;

struct AuthDiffPowerEventScenario {
    events: HashMap<String, LeanEvent>,
    states: Vec<StateMap>,
    name_key: (EventType, String),
    power_levels_key: (EventType, String),
}

fn auth_diff_power_event_scenario(room: &str) -> AuthDiffPowerEventScenario {
    let mut events = utils::parse_jsonl_events(room);
    events.extend(utils::parse_jsonl_events(AUTH_DIFF_POWER_EVENT_BRANCH));
    let events = to_event_map(&events);

    let mut states = fork_states();
    for state in &mut states {
        state.insert(bob_key(), "$bob_join".to_string());
    }
    let name_key = (EventType::from("m.room.name"), String::new());
    states[0].insert(name_key.clone(), "$name_alice".to_string());
    states[1].insert(name_key.clone(), "$name_bob".to_string());
    let pl_key = (EventType::from("m.room.power_levels"), String::new());

    AuthDiffPowerEventScenario {
        events,
        states,
        name_key,
        power_levels_key: pl_key,
    }
}

fn resolve_concrete_and_lazy(
    states: &[StateMap],
    events: &HashMap<String, LeanEvent>,
    version: StateResVersion,
) -> [(&'static str, StateMap); 2] {
    [
        (
            "concrete",
            rezzy::resolve_state_maps(states, events, version),
        ),
        (
            "lazy",
            resolve_state_maps_lazy_with_diff(states, events, None::<Vec<String>>, version),
        ),
    ]
}

#[test]
fn test_v2_auth_difference_power_event_authorizes_conflicted_event() {
    let scenario = auth_diff_power_event_scenario(ROOM);
    for (path, resolved) in
        resolve_concrete_and_lazy(&scenario.states, &scenario.events, StateResVersion::V2)
    {
        assert_eq!(
            resolved.get(&scenario.name_key).map(String::as_str),
            Some("$name_bob"),
            "{path}"
        );
        assert_eq!(
            resolved.get(&scenario.power_levels_key).map(String::as_str),
            Some("$pl"),
            "{path}"
        );
    }
}

/// In V1, the auth difference (∪Cᵢ − ∩Cᵢ) is never computed or added to the
/// conflicted events set. An auth-difference-only event (such as `$pl_diff`
/// cited only in Bob's auth chain) is excluded from resolution, so Bob's name
/// lacks authorization and Alice's name wins.
#[test]
fn test_v1_does_not_include_auth_difference_events() {
    let scenario = auth_diff_power_event_scenario(ROOM);
    for (path, resolved) in
        resolve_concrete_and_lazy(&scenario.states, &scenario.events, StateResVersion::V1)
    {
        // Alice has PL 100 in unconflicted $pl; Bob lacks power because $pl_diff is not in V1
        assert_eq!(
            resolved.get(&scenario.name_key).map(String::as_str),
            Some("$name_alice"),
            "{path}"
        );
        assert_eq!(
            resolved.get(&scenario.power_levels_key).map(String::as_str),
            Some("$pl"),
            "{path}"
        );
    }

    // Also verify compute_state_at (src/state/at.rs) gates out auth difference for V1.
    // In events_with_merge, Bob's name only cites $pl_diff in auth_events (not prev_events),
    // so both parents agree on $pl in their state, and $pl_diff is purely an auth-difference event.
    let mut events_with_merge = scenario.events.clone();
    if let Some(bob_name) = events_with_merge.get_mut("$name_bob") {
        bob_name.prev_events = vec!["$bob_join".to_string()];
    }
    events_with_merge.insert(
        "$merge".to_string(),
        utils::parse_jsonl_events(
            r#"{"event_id":"$merge","type":"m.room.message","sender":"@alice:a.com","depth":7,"origin_server_ts":1800,"prev_events":["$name_alice","$name_bob"],"auth_events":["$create","$alice_join"]}"#,
        )
        .remove(0),
    );
    let state_at_merge = rezzy::compute_state_at(
        "$merge",
        &events_with_merge,
        StateResVersion::V1,
        &String::new(),
    )
    .expect("merge state must resolve");
    assert_eq!(
        state_at_merge.get(&scenario.name_key).map(String::as_str),
        Some("$name_alice"),
        "compute_state_at under V1"
    );
    assert_eq!(
        state_at_merge
            .get(&scenario.power_levels_key)
            .map(String::as_str),
        Some("$pl"),
        "compute_state_at under V1"
    );
}

const ROOM_V12: &str = r#"
{"event_id":"$create","type":"m.room.create","state_key":"","sender":"@alice:a.com","depth":0,"origin_server_ts":1000,"content":{"room_version":"12"},"prev_events":[],"auth_events":[]}
{"event_id":"$alice_join","type":"m.room.member","state_key":"@alice:a.com","sender":"@alice:a.com","depth":1,"origin_server_ts":1001,"content":{"membership":"join"},"prev_events":["$create"],"auth_events":["$create"]}
{"event_id":"$pl","type":"m.room.power_levels","state_key":"","sender":"@alice:a.com","depth":2,"origin_server_ts":1002,"content":{"users":{"@alice:a.com":100},"events_default":0,"state_default":50,"ban":50,"kick":50,"invite":0},"prev_events":["$alice_join"],"auth_events":["$create","$alice_join"]}
{"event_id":"$jr","type":"m.room.join_rules","state_key":"","sender":"@alice:a.com","depth":3,"origin_server_ts":1003,"content":{"join_rule":"public"},"prev_events":["$pl"],"auth_events":["$create","$alice_join","$pl"]}
{"event_id":"$bob_join","type":"m.room.member","state_key":"@bob:b.com","sender":"@bob:b.com","depth":4,"origin_server_ts":1010,"content":{"membership":"join"},"prev_events":["$jr"],"auth_events":["$create","$pl","$jr"]}
"#;

/// In Room v12 / MSC4297 (`StateResVersion::V2_1` and `StateResVersion::V2_1_1`), Synapse applies
/// auth-difference power events during `_iterative_auth_checks`, and restores
/// the unconflicted state map at the end (via ChainMap(unconflicted, resolved)).
///
#[test]
fn test_v2_1_auth_difference_power_event_authorizes_conflicted_event() {
    let scenario = auth_diff_power_event_scenario(ROOM_V12);
    for version in [StateResVersion::V2_1, StateResVersion::V2_1_1] {
        for (path, resolved) in
            resolve_concrete_and_lazy(&scenario.states, &scenario.events, version)
        {
            assert_eq!(
                resolved.get(&scenario.name_key).map(String::as_str),
                Some("$name_bob"),
                "{version:?} {path}"
            );
            assert_eq!(
                resolved.get(&scenario.power_levels_key).map(String::as_str),
                Some("$pl"),
                "{version:?} {path}"
            );
        }
    }

    // The semilattice entry point delegates V2.1+ to the iterative resolver;
    // provide the auth-difference event explicitly and verify the final merge
    // restores agreed power levels.
    let mut semilattice_unconflicted = scenario.states[0].clone();
    semilattice_unconflicted.remove(&scenario.name_key);
    let semilattice = resolve_semilattice_fold(
        &semilattice_unconflicted,
        &HashMap::from([
            (
                "$name_alice".to_string(),
                scenario.events["$name_alice"].clone(),
            ),
            (
                "$name_bob".to_string(),
                scenario.events["$name_bob"].clone(),
            ),
            ("$pl_diff".to_string(), scenario.events["$pl_diff"].clone()),
        ]),
        &scenario.events,
        StateResVersion::V2_1,
    );
    assert_eq!(
        semilattice.get(&scenario.name_key).map(String::as_str),
        Some("$name_bob"),
        "semilattice entry point"
    );
    assert_eq!(
        semilattice
            .get(&scenario.power_levels_key)
            .map(String::as_str),
        Some("$pl"),
        "semilattice entry point restores unconflicted power levels"
    );
}

// ============================================================================
// AuthGraph: bitmap auth difference
// ============================================================================

/// Builds the graph event by event, in the topological order `build` uses.
fn graph_by_insert(events: &HashMap<String, LeanEvent>) -> rezzy::auth::roaring::AuthGraph {
    let built = rezzy::auth::roaring::AuthGraph::build(events);
    let mut graph = rezzy::auth::roaring::AuthGraph::build(&HashMap::<String, LeanEvent>::new());
    for id in built.index.items() {
        assert_eq!(graph.insert(&events[id]), Ok(true));
    }
    assert_eq!(
        graph.insert(&events[&built.index.items()[0]]),
        Ok(false),
        "re-insert is a no-op"
    );
    assert_eq!(graph.auth_bitmaps, built.auth_bitmaps);
    graph
}

#[test]
fn test_auth_graph_state_difference_excludes_shared_ancestor() {
    let map = shared_ancestor_room();
    let diff = graph_by_insert(&map).state_auth_difference(&shared_ancestor_forks());
    assert!(!diff.contains(&"$x".to_string()), "{diff:?}");
    assert!(!diff.contains(&"$bob_join".to_string()), "{diff:?}");
}

/// The bitmap diff, passed as the precomputed diff, resolves the same as the
/// lazy resolver computing the diff itself.
#[test]
fn test_auth_graph_state_difference_matches_computed_diff() {
    for (name, map, forks) in [
        ("tied chain", room_with_chain(true), fork_states()),
        ("distinct chain", room_with_chain(false), fork_states()),
        (
            "shared ancestor",
            shared_ancestor_room(),
            shared_ancestor_forks(),
        ),
    ] {
        let diff = graph_by_insert(&map).state_auth_difference(&forks);
        let with_graph =
            resolve_state_maps_lazy_with_diff(&forks, &map, Some(diff), StateResVersion::V2);
        let computed = resolve_state_maps_lazy_with_diff(
            &forks,
            &map,
            None::<Vec<String>>,
            StateResVersion::V2,
        );
        assert_eq!(with_graph, computed, "{name}");
    }
}

/// An event whose auth events are not in the graph yet is refused, and the
/// graph is unchanged, rather than getting a bitmap that a later parent could
/// never repair.
#[test]
fn test_auth_graph_insert_refuses_missing_auth_events() {
    let map = room_with_chain(true);
    let mut graph = rezzy::auth::roaring::AuthGraph::build(&HashMap::<String, LeanEvent>::new());
    assert_eq!(graph.insert(&map["$create"]), Ok(true));

    let err = graph.insert(&map["$m_a"]).unwrap_err();
    let rezzy::auth::roaring::InsertError::Missing(missing) = err else {
        panic!("expected missing auth events");
    };
    let mut missing = missing.missing;
    missing.sort();
    assert_eq!(missing, ["$bob_join", "$jr", "$pl"]);
    assert_eq!(
        graph.index.len(),
        1,
        "a refused insert leaves the graph unchanged"
    );
}

/// A graph built from an event set that is not auth-closed is refused, naming
/// the missing auth events; the lenient `build` would skip them silently.
#[test]
fn test_auth_graph_try_build_refuses_missing_auth_events() {
    let mut map = room_with_chain(true);
    map.remove("$pl");
    let err = rezzy::auth::roaring::AuthGraph::try_build(&map)
        .err()
        .expect("$pl is cited but absent");
    let rezzy::auth::roaring::BuildError::Missing(missing) = err else {
        panic!("expected missing auth events");
    };
    assert_eq!(missing.missing, ["$pl"]);
    assert!(rezzy::auth::roaring::AuthGraph::try_build(&room_with_chain(true)).is_ok());
}

/// A state event missing from the graph is refused rather than leaving its
/// chain out of the difference.
#[test]
fn test_auth_graph_try_state_difference_refuses_unknown_state_events() {
    let map = room_with_chain(true);
    let graph = rezzy::auth::roaring::AuthGraph::try_build(&map).unwrap();
    let mut forks = fork_states();
    forks[1].insert(bob_key(), "$not_in_graph".to_string());
    let err = graph.try_state_auth_difference(&forks).unwrap_err();
    assert_eq!(err.missing, ["$not_in_graph"]);
    assert_eq!(
        graph.try_state_auth_difference(&fork_states()).unwrap(),
        graph.state_auth_difference(&fork_states())
    );
}

// ============================================================================
// AuthGraph: hybrid (graph + provider walk) auth difference
// ============================================================================

const BASE: [&str; 5] = ["$create", "$alice_join", "$pl", "$jr", "$bob_join"];

/// A complete graph over just `ids` (which must be auth-closed).
fn graph_of(map: &HashMap<String, LeanEvent>, ids: &[&str]) -> rezzy::auth::roaring::AuthGraph {
    let subset: HashMap<String, LeanEvent> = ids
        .iter()
        .map(|id| ((*id).to_string(), map[*id].clone()))
        .collect();
    rezzy::auth::roaring::AuthGraph::try_build(&subset).expect("subset is auth-closed")
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// Indexed-only, walk-only and mixed graphs all give the full graph's diff.
#[test]
fn test_hybrid_difference_matches_full_graph() {
    for (name, map, forks) in [
        ("tied chain", room_with_chain(true), fork_states()),
        (
            "shared ancestor",
            shared_ancestor_room(),
            shared_ancestor_forks(),
        ),
    ] {
        let full = rezzy::auth::roaring::AuthGraph::try_build(&map).unwrap();
        let expected = sorted(full.state_auth_difference(&forks));
        let empty = graph_of(&map, &[]);
        let base = graph_of(&map, &BASE);
        for (graph_name, graph) in [
            ("indexed-only", &full),
            ("walk-only", &empty),
            ("mixed", &base),
        ] {
            assert_eq!(
                graph.hybrid_state_auth_difference(&forks, &map).unwrap(),
                expected,
                "{name}, {graph_name}"
            );
        }
    }
}

/// An unindexed ancestor reached by every fork's walk is in the intersection.
#[test]
fn test_hybrid_difference_excludes_shared_unindexed_ancestor() {
    let map = shared_ancestor_room();
    let diff = graph_of(&map, &BASE)
        .hybrid_state_auth_difference(&shared_ancestor_forks(), &map)
        .unwrap();
    assert!(!diff.contains(&"$x".to_string()), "{diff:?}");
}

#[test]
fn test_hybrid_difference_reports_missing_auth_event() {
    let mut map = room_with_chain(true);
    let graph = graph_of(&map, &BASE);
    map.remove("$m_c");
    let err = graph
        .hybrid_state_auth_difference(&fork_states(), &map)
        .unwrap_err();
    assert_eq!(
        err,
        rezzy::auth::roaring::AuthDifferenceError::Missing(rezzy::auth::roaring::MissingEvents {
            missing: vec!["$m_c".to_string()],
        })
    );
}

/// A lenient build that skipped an auth event is refused: the walk could not
/// tell that an indexed bitmap lacks an ancestor.
#[test]
fn test_hybrid_difference_refuses_incomplete_graph() {
    let map = room_with_chain(true);
    let mut partial = map.clone();
    partial.remove("$pl");
    let graph = rezzy::auth::roaring::AuthGraph::build(&partial);
    assert!(!graph.is_complete());
    assert!(rezzy::auth::roaring::AuthGraph::try_build(&map)
        .unwrap()
        .is_complete());
    assert_eq!(
        graph.hybrid_state_auth_difference(&fork_states(), &map),
        Err(rezzy::auth::roaring::AuthDifferenceError::IncompleteGraph)
    );
}

/// Counts lookups per event ID.
struct CountingMap<'a> {
    events: &'a HashMap<String, LeanEvent>,
    lookups: std::cell::RefCell<HashMap<String, usize>>,
}

impl rezzy::basespec::rezzy_types::EventProvider<String, rezzy::JsonValue> for CountingMap<'_> {
    fn get_event(&self, id: &String) -> Option<&LeanEvent> {
        let mut lookups = self.lookups.borrow_mut();
        let n = lookups.entry(id.clone()).or_default();
        *n = n.saturating_add(1);
        self.events.get(id)
    }
}

/// Every fork's walk reaches the absent `$create`, but it is looked up once
/// and reported once; present events are also fetched once across forks.
#[test]
fn test_hybrid_difference_looks_up_each_event_once() {
    let mut map = room_with_chain(true);
    map.remove("$create");
    let provider = CountingMap {
        events: &map,
        lookups: std::cell::RefCell::default(),
    };
    let graph = graph_of(&map, &[]);
    let err = graph
        .hybrid_state_auth_difference(&fork_states(), &provider)
        .unwrap_err();
    assert_eq!(
        err,
        rezzy::auth::roaring::AuthDifferenceError::Missing(rezzy::auth::roaring::MissingEvents {
            missing: vec!["$create".to_string()],
        })
    );
    let lookups = provider.lookups.borrow();
    assert_eq!(lookups["$create"], 1);
    assert!(lookups.values().all(|&n| n == 1), "{lookups:?}");
}
