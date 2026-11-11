//! Cold-read cost of type-scoped state reads: the flat state HAMT versus the
//! best case a typed (`MTHR` 0x02) layout could reach.
//!
//! No typed format exists. The "lane" numbers come from ordinary flat HAMTs
//! built over just that lane's entries, which is exactly what a typed lane
//! subtree would be, minus the directory. See
//! `docs/2026-11-11-typed-hamt-roots.md` for the decision this feeds.
//!
//! For each room shape and size it reports:
//! - the per-type histogram of the fixture;
//! - cold scans (fresh `PersistedHamtReader`, every node fetched and verified)
//!   for: all state, `m.room.member` filtered out of the flat tree versus a
//!   members-only lane, and `m.space.child` filtered out of the flat tree
//!   versus the `other` lane versus an exact per-type subtree;
//! - auth-singleton point reads: one fresh reader per read with the top
//!   0/1/2 tree levels treated as resident (a bounded node cache shared across
//!   state groups), and one shared reader before and after its cache warms,
//!   versus an inline sorted `singles` record (0 fetches);
//! - bytes newly written per member insert, flat versus members lane;
//! - in-memory baselines: `PersistentOrdMap` range scans and point reads, and
//!   an in-memory flat HAMT filter scan.
//!
//! Fetch and byte counts are the result; times are against an in-memory byte
//! store, so they measure decode + verification, not device latency.
//!
//! Run with: `cargo bench --bench rezzy -- hamt_type_scan`

use std::cell::Cell;
use std::collections::HashSet;
use std::convert::Infallible;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Instant;

use rezzy::basespec::event_types::EventType;
use rezzy::hamt::codec::HamtCodec;
use rezzy::hamt::hash::StructuralHash;
use rezzy::hamt::{self, HamtKey, HamtNode, HamtValue, NodeRef, PersistedHamtReader};
use rezzy::PersistentOrdMap;

use crate::common::prelude::*;
use crate::common::{
    collect_all_nodes, collect_new_nodes, measure, to_persisted, unreachable_resolver, Xorshift128,
};

type Key = (EventType, String);
type Value = String;
type Store = HashMap<StructuralHash, Vec<u8>>;

const STRUCTURAL_KEY: &[u8] = b"bench-hamt-type-scan";
const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";

/// The fixed allowlist a typed root would inline (`state_key = ""` only).
const AUTH_SINGLETONS: &[EventType] = &[
    EventType::RoomCreate,
    EventType::RoomPowerLevels,
    EventType::RoomJoinRules,
    EventType::RoomHistoryVisibility,
    EventType::RoomGuestAccess,
];

/// Singleton config types that are not auth-critical, so they stay in `other`.
const OTHER_SINGLETONS: &[EventType] = &[
    EventType::RoomName,
    EventType::RoomTopic,
    EventType::RoomAvatar,
    EventType::RoomEncryption,
    EventType::RoomCanonicalAlias,
];

/// Room shape as percentage weights for the non-singleton entries.
struct Shape {
    name: &'static str,
    members: u64,
    space_child: u64,
    call_member: u64,
    /// Each custom entry is its own single-entry type (the bloat shape).
    custom: u64,
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "member-heavy",
        members: 97,
        space_child: 1,
        call_member: 1,
        custom: 1,
    },
    Shape {
        name: "typical",
        members: 85,
        space_child: 5,
        call_member: 5,
        custom: 5,
    },
    Shape {
        name: "custom-heavy",
        members: 30,
        space_child: 5,
        call_member: 5,
        custom: 60,
    },
];

fn event_id(rng: &mut Xorshift128) -> Value {
    format!("${:016x}:example.org", rng.next_u64())
}

/// Builds `n` distinct entries: every singleton once, then the shape's mix.
fn make_fixture(shape: &Shape, n: usize, seed: u64) -> Vec<(Key, Value)> {
    assert_eq!(
        shape.members + shape.space_child + shape.call_member + shape.custom,
        100
    );
    let mut rng = Xorshift128::new(seed);
    let mut used = HashSet::new();
    let mut out = Vec::with_capacity(n);
    for ty in AUTH_SINGLETONS.iter().chain(OTHER_SINGLETONS) {
        let key = (ty.clone(), String::new());
        used.insert(key.clone());
        out.push((key, event_id(&mut rng)));
    }
    let call = EventType::from(CALL_MEMBER);
    let mut custom_next = 0_u64;
    let (m, s, c) = (
        shape.members,
        shape.members + shape.space_child,
        shape.members + shape.space_child + shape.call_member,
    );
    while out.len() < n {
        let roll = rng.next_u64() % 100;
        let id = rng.next_u64() % (n as u64 * 4);
        let key = if roll < m {
            (EventType::RoomMember, format!("@user{id}:example.org"))
        } else if roll < s {
            (EventType::SpaceChild, format!("!room{id}:example.org"))
        } else if roll < c {
            (call.clone(), format!("_@user{id}:example.org_DEVICE"))
        } else {
            custom_next += 1;
            (
                EventType::from(format!("org.example.custom{custom_next}")),
                String::new(),
            )
        };
        if used.insert(key.clone()) {
            let value = event_id(&mut rng);
            out.push((key, value));
        }
    }
    out
}

fn is_auth_singleton(key: &Key) -> bool {
    key.1.is_empty() && AUTH_SINGLETONS.contains(&key.0)
}

/// Encodes every node of a fully resolved tree into a byte store.
fn persist<K, V>(root: &Arc<HamtNode<K, V>>) -> Store
where
    K: HamtCodec + Clone,
    V: HamtCodec + Clone,
{
    let mut nodes = Vec::new();
    collect_all_nodes(root, &mut nodes);
    nodes
        .iter()
        .map(|node| (node.structural_hash, to_persisted(node).encode_v1()))
        .collect()
}

#[derive(Clone, Copy, Default)]
struct Cost {
    fetches: f64,
    bytes: f64,
    matched: usize,
    ns: f64,
}

/// Node fetches and bytes a [`counting_loader`] has served.
#[derive(Default)]
struct Fetched {
    nodes: Cell<usize>,
    bytes: Cell<usize>,
}

/// A loader over `store` that counts every fetch of a node outside `resident`.
fn counting_loader<'a>(
    store: &'a Store,
    resident: &'a HashSet<StructuralHash>,
    fetched: &'a Fetched,
) -> impl FnMut(&StructuralHash) -> Result<Option<Vec<u8>>, Infallible> + 'a {
    move |hash| {
        let record = store.get(hash).cloned();
        if let Some(record) = &record {
            if !resident.contains(hash) {
                fetched.nodes.set(fetched.nodes.get() + 1);
                fetched.bytes.set(fetched.bytes.get() + record.len());
            }
        }
        Ok(record)
    }
}

/// Cold scan: a fresh reader per rep, so every node is fetched and verified.
fn cold_scan<K, V>(
    store: &Store,
    root: StructuralHash,
    reps: u32,
    keep: impl Fn(&K) -> bool,
) -> Cost
where
    K: HamtCodec + Hash,
    V: HamtCodec + Hash,
{
    let mut cost = Cost::default();
    let start = Instant::now();
    for _ in 0..reps {
        let (fetched, resident, mut matched) = (Fetched::default(), HashSet::new(), 0);
        {
            let mut reader = PersistedHamtReader::<K, V, Infallible, _>::new(
                STRUCTURAL_KEY,
                counting_loader(store, &resident, &fetched),
            );
            reader
                .for_each_entry(root, |key, _| {
                    if keep(key) {
                        matched += 1;
                    }
                })
                .expect("bench store holds every node");
        }
        black_box(matched);
        cost = Cost {
            fetches: fetched.nodes.get() as f64,
            bytes: fetched.bytes.get() as f64,
            matched,
            ns: 0.0,
        };
    }
    cost.ns = start.elapsed().as_nanos() as f64 / f64::from(reps);
    cost
}

/// Hashes of the nodes in the top `levels` levels (0 = nothing resident).
fn warm_set<K, V>(node: &Arc<HamtNode<K, V>>, levels: usize, out: &mut HashSet<StructuralHash>) {
    if levels == 0 {
        return;
    }
    out.insert(node.structural_hash);
    for child in &node.children {
        if let NodeRef::Resolved(c) = child {
            warm_set(c, levels - 1, out);
        }
    }
}

/// Average fetches and bytes per point read with one fresh reader each,
/// counting only nodes outside `warm`. Times include re-decoding warm nodes,
/// so only the counts are meaningful for the warm rows.
fn cold_point_reads(
    store: &Store,
    root: StructuralHash,
    warm: &HashSet<StructuralHash>,
    keys: &[Key],
    reps: u32,
) -> Cost {
    let fetched = Fetched::default();
    let start = Instant::now();
    for _ in 0..reps {
        for key in keys {
            let mut reader = PersistedHamtReader::<Key, Value, Infallible, _>::new(
                STRUCTURAL_KEY,
                counting_loader(store, warm, &fetched),
            );
            let value = reader.get(root, key).expect("bench store holds every node");
            assert!(value.is_some(), "auth singleton must be present");
            black_box(value);
        }
    }
    let reads = (keys.len() as u32 * reps) as usize;
    Cost {
        fetches: fetched.nodes.get() as f64 / reads as f64,
        bytes: fetched.bytes.get() as f64 / reads as f64,
        matched: keys.len(),
        ns: start.elapsed().as_nanos() as f64 / reads as f64,
    }
}

/// Reads all auth keys through one reader, then measures the same reads after
/// the reader's real decoded-node cache has warmed.
fn shared_reader_point_reads(
    store: &Store,
    root: StructuralHash,
    keys: &[Key],
) -> (Cost, usize, f64) {
    let (fetched, resident) = (Fetched::default(), HashSet::new());
    let mut reader = PersistedHamtReader::<Key, Value, Infallible, _>::new(
        STRUCTURAL_KEY,
        counting_loader(store, &resident, &fetched),
    );
    let cold_start = Instant::now();
    for key in keys {
        let value = reader.get(root, key).expect("bench store holds every node");
        assert!(value.is_some(), "auth singleton must be present");
        black_box(value);
    }
    let cold_elapsed = cold_start.elapsed();
    let (cold_fetches, cold_bytes) = (fetched.nodes.get(), fetched.bytes.get());

    let warm_reps = 1000_u32;
    let warm_start = Instant::now();
    for _ in 0..warm_reps {
        for key in keys {
            black_box(
                reader
                    .get(root, key)
                    .expect("cached point read should succeed"),
            );
        }
    }
    let warm_ns = warm_start.elapsed().as_nanos() as f64 / f64::from(warm_reps) / keys.len() as f64;
    let warm_fetches = fetched.nodes.get() - cold_fetches;
    (
        Cost {
            fetches: cold_fetches as f64,
            bytes: cold_bytes as f64,
            matched: keys.len(),
            ns: cold_elapsed.as_nanos() as f64 / keys.len() as f64,
        },
        warm_fetches,
        warm_ns,
    )
}

/// Average bytes of newly allocated nodes per single path-copied insert.
fn write_bytes_per_insert<K, V>(root: &Arc<HamtNode<K, V>>, inserts: Vec<(K, V)>) -> f64
where
    K: HamtKey,
    V: HamtValue,
{
    let count = inserts.len();
    let mut total = 0_usize;
    for (key, value) in inserts {
        let mut resolver = unreachable_resolver();
        let (new_root, _) = hamt::insert(root, STRUCTURAL_KEY, key, value, &mut resolver)
            .expect("insert should not collide");
        let mut fresh = Vec::new();
        collect_new_nodes(root, &new_root, &mut fresh);
        total += fresh
            .iter()
            .map(|node| to_persisted(node).encode_v1().len())
            .sum::<usize>();
    }
    total as f64 / count as f64
}

fn ratio(flat: f64, lane: f64) -> f64 {
    flat / lane.max(1.0)
}

fn print_scan(label: &str, cost: Cost) {
    println!(
        "    {label:<34} matched {:>7}  fetches {:>6.2}  bytes {:>10.1}  {:>10.1} us",
        cost.matched,
        cost.fetches,
        cost.bytes,
        cost.ns / 1000.0
    );
}

fn bench(shape: &Shape, n: usize) {
    let entries = make_fixture(shape, n, 0x7E5C_A000 + n as u64);
    let reps: u32 = if n >= 50_000 { 3 } else { 20 };
    let call = EventType::from(CALL_MEMBER);

    // Histogram.
    let members = entries
        .iter()
        .filter(|(k, _)| k.0 == EventType::RoomMember)
        .count();
    let space = entries
        .iter()
        .filter(|(k, _)| k.0 == EventType::SpaceChild)
        .count();
    let calls = entries.iter().filter(|(k, _)| k.0 == call).count();
    let singles = AUTH_SINGLETONS.len() + OTHER_SINGLETONS.len();
    let custom = entries.len() - members - space - calls - singles;
    println!("\n== {} n={n} ==", shape.name);
    println!(
        "  histogram: member {members} ({:.1}%), m.space.child {space}, call.member {calls}, \
         singletons {singles}, custom single-entry types {custom}",
        100.0 * members as f64 / n as f64
    );

    // Flat tree.
    let flat = hamt::build_hamt::<Key, Value, _>(STRUCTURAL_KEY, entries.iter().cloned())
        .expect("build should not collide");
    let flat_store = persist(&flat);

    // Lanes: members keyed by state_key, other keyed by (type, key), plus an
    // exact m.space.child subtree as the per-type-directory lower bound.
    let member_lane: Vec<(String, Value)> = entries
        .iter()
        .filter(|(k, _)| k.0 == EventType::RoomMember)
        .map(|((_, sk), v)| (sk.clone(), v.clone()))
        .collect();
    let other_lane: Vec<(Key, Value)> = entries
        .iter()
        .filter(|(k, _)| k.0 != EventType::RoomMember && !is_auth_singleton(k))
        .cloned()
        .collect();
    let space_lane: Vec<(String, Value)> = entries
        .iter()
        .filter(|(k, _)| k.0 == EventType::SpaceChild)
        .map(|((_, sk), v)| (sk.clone(), v.clone()))
        .collect();
    let mut singles_record: Vec<(EventType, Value)> = entries
        .iter()
        .filter(|(k, _)| is_auth_singleton(k))
        .map(|((t, _), v)| (t.clone(), v.clone()))
        .collect();
    singles_record.sort();

    let members_root =
        hamt::build_hamt::<String, Value, _>(STRUCTURAL_KEY, member_lane.iter().cloned())
            .expect("build should not collide");
    let other_root = hamt::build_hamt::<Key, Value, _>(STRUCTURAL_KEY, other_lane.iter().cloned())
        .expect("build should not collide");
    let space_root =
        hamt::build_hamt::<String, Value, _>(STRUCTURAL_KEY, space_lane.iter().cloned())
            .expect("build should not collide");
    let members_store = persist(&members_root);
    let other_store = persist(&other_root);
    let space_store = persist(&space_root);
    println!(
        "  nodes: flat {}, members lane {}, other lane {}, space.child subtree {}",
        flat_store.len(),
        members_store.len(),
        other_store.len(),
        space_store.len()
    );

    // Cold scans.
    println!("  cold scans:");
    let full = cold_scan::<Key, Value>(&flat_store, flat.structural_hash, reps, |_| true);
    print_scan("flat: all state", full);

    let flat_members = cold_scan::<Key, Value>(&flat_store, flat.structural_hash, reps, |k| {
        k.0 == EventType::RoomMember
    });
    let lane_members =
        cold_scan::<String, Value>(&members_store, members_root.structural_hash, reps, |_| true);
    print_scan("flat: filter m.room.member", flat_members);
    print_scan("lane: members", lane_members);
    println!(
        "    -> member scan flat/lane: fetches {:.2}x, bytes {:.2}x",
        ratio(flat_members.fetches, lane_members.fetches),
        ratio(flat_members.bytes, lane_members.bytes)
    );

    let flat_space = cold_scan::<Key, Value>(&flat_store, flat.structural_hash, reps, |k| {
        k.0 == EventType::SpaceChild
    });
    let other_space =
        cold_scan::<Key, Value>(&other_store, other_root.structural_hash, reps, |k| {
            k.0 == EventType::SpaceChild
        });
    let exact_space =
        cold_scan::<String, Value>(&space_store, space_root.structural_hash, reps, |_| true);
    print_scan("flat: filter m.space.child", flat_space);
    print_scan("other lane: filter m.space.child", other_space);
    print_scan("exact subtree: m.space.child", exact_space);
    println!(
        "    -> space.child flat/other: fetches {:.2}x; flat/exact: fetches {:.2}x",
        ratio(flat_space.fetches, other_space.fetches),
        ratio(flat_space.fetches, exact_space.fetches)
    );

    // Auth singleton point reads.
    println!("  auth singleton point reads:");
    let auth_keys: Vec<Key> = AUTH_SINGLETONS
        .iter()
        .map(|t| (t.clone(), String::new()))
        .collect();
    for levels in 0..=2 {
        let mut warm = HashSet::new();
        warm_set(&flat, levels, &mut warm);
        let cold = cold_point_reads(&flat_store, flat.structural_hash, &warm, &auth_keys, reps);
        println!(
            "    flat, fresh reader, top {levels} warm:     fetches {:>5.1}  bytes {:>7.1}  {:>8.0} ns/read",
            cold.fetches, cold.bytes, cold.ns
        );
    }
    let (shared, warm_fetches, warm_ns) =
        shared_reader_point_reads(&flat_store, flat.structural_hash, &auth_keys);
    println!(
        "    flat, shared reader first batch:   fetches {:>5.1}  bytes {:>7.1}  {:>8.0} ns/read",
        shared.fetches, shared.bytes, shared.ns
    );
    println!(
        "    flat, shared reader warmed:        fetches {warm_fetches:>3}  {:>18.0} ns/read",
        warm_ns
    );
    let singles_reads = AUTH_SINGLETONS.len() as u32 * reps * 100;
    let elapsed = measure(reps * 100, || {
        for ty in AUTH_SINGLETONS {
            let hit = singles_record.binary_search_by(|(t, _)| t.cmp(ty));
            black_box(hit.expect("auth singleton must be present"));
        }
    });
    println!(
        "    singles record (in root):        fetches 0  bytes      0  {:>8.1} ns",
        elapsed.as_nanos() as f64 / f64::from(singles_reads)
    );

    // Write amplification per member insert.
    let mut rng = Xorshift128::new(0xB17E_5000 + n as u64);
    let new_members: Vec<String> = (0..64)
        .map(|i| format!("@new{i}_{}:example.org", rng.next_u64()))
        .collect();
    let flat_write = write_bytes_per_insert(
        &flat,
        new_members
            .iter()
            .map(|sk| {
                (
                    (EventType::RoomMember, sk.clone()),
                    "$new:example.org".to_string(),
                )
            })
            .collect(),
    );
    let lane_write = write_bytes_per_insert(
        &members_root,
        new_members
            .iter()
            .map(|sk| (sk.clone(), "$new:example.org".to_string()))
            .collect(),
    );
    println!(
        "  bytes written per member insert: flat {flat_write:.0}, members lane {lane_write:.0} \
         (+ directory/root record, not counted)"
    );

    // In-memory baselines.
    println!("  in-memory baselines:");
    let ord: PersistentOrdMap<Key, Value> = entries.iter().cloned().collect();
    let type_range = |ty: &EventType| {
        ord.range((ty.clone(), String::new())..)
            .take_while(|(k, _)| &k.0 == ty)
            .count()
    };
    let scan_reps = reps * 10;
    for (label, ty) in [
        (
            "PersistentOrdMap range m.room.member",
            EventType::RoomMember,
        ),
        (
            "PersistentOrdMap range m.space.child",
            EventType::SpaceChild,
        ),
    ] {
        let mut matched = 0;
        let elapsed = measure(scan_reps, || {
            matched = black_box(type_range(&ty));
        });
        println!(
            "    {label:<40} matched {matched:>7}  {:>10.1} us",
            elapsed.as_nanos() as f64 / f64::from(scan_reps) / 1000.0
        );
    }
    let mut matched = 0;
    let elapsed = measure(scan_reps, || {
        let mut count = 0_usize;
        let mut resolver = unreachable_resolver();
        flat.visit_entries(&mut resolver, &mut |k: &Key,
                                                _v: &Value|
         -> Result<(), ()> {
            if k.0 == EventType::RoomMember {
                count += 1;
            }
            Ok(())
        })
        .expect("in-memory tree is fully resolved");
        matched = black_box(count);
    });
    println!(
        "    {:<40} matched {matched:>7}  {:>10.1} us",
        "flat HAMT in memory, filter member",
        elapsed.as_nanos() as f64 / f64::from(scan_reps) / 1000.0
    );
    let point_reps = reps * 100;
    let elapsed = measure(point_reps, || {
        for key in &auth_keys {
            black_box(ord.get(key).expect("auth singleton must be present"));
        }
    });
    println!(
        "    {:<40} {:>10.1} ns/read",
        "PersistentOrdMap get auth singleton",
        elapsed.as_nanos() as f64 / f64::from(point_reps * auth_keys.len() as u32)
    );
}

pub fn run() {
    for shape in SHAPES {
        for &n in &[1_000_usize, 10_000, 100_000] {
            bench(shape, n);
        }
    }
}
