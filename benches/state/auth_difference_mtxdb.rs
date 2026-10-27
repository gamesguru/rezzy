//! The `auth_difference` fixture stored in mtxdb.
//!
//! Events are written as JSON records to mtxdb's event pool and their `auth`
//! edges to its adjacency, then `AuthClosure::rebuild` persists their closures.
//! Resolution reads events back through an mtxdb-backed provider, and the auth
//! difference comes from the persisted closures, as a server on mtxdb would
//! compute it. Reads are served from the OS page cache after the first run, so
//! these are warm-cache numbers.
//!
//! Run with `cargo +1.90.0 bench --manifest-path benches/Cargo.toml --bench
//! rezzy --features mtxdb -- auth_difference_mtxdb`.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use mtxdb::auth_closure::AuthClosure;
use mtxdb::database::Database;
use mtxdb::matrix_adjacency::MatrixAdjacency;
use mtxdb::storage::{NodeData, StorageEngine};
use mtxdb::{BitmapSet, ShardType};
use rezzy::basespec::rezzy_types::EventProvider;
use rezzy::{JsonValue, LeanEvent, StateResVersion};

use super::auth_difference::{build_fixture, Fixture, StateMap};

const ROOM: &str = "!bench:example.com";
const EVENTS_COLLECTION: [u8; 16] = *b"bench-room-evts\0";

/// Derives a fixture storage key from the first 16 bytes of the event ID's
/// BLAKE3 digest.
fn node_id(event_id: &str) -> [u8; 16] {
    let digest = blake3::hash(event_id.as_bytes());
    let mut id = [0; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    id
}

/// Serializes fixture event fields for storage, using an empty state key when
/// absent and an empty `prev_events` list.
fn pdu_json(ev: &LeanEvent) -> String {
    rezzy::json!({
        "event_id": ev.event_id.clone(),
        "type": ev.event_type.clone(),
        "state_key": ev.state_key.clone().unwrap_or_default(),
        "sender": ev.sender.clone(),
        "depth": ev.depth,
        "origin_server_ts": ev.origin_server_ts,
        "content": ev.content.clone(),
        "auth_events": ev.auth_events.clone(),
        "prev_events": Vec::<String>::new(),
    })
    .to_string()
}

/// A room stored in a fresh mtxdb database.
struct Store {
    root: PathBuf,
    db: Option<Database>,
    closure: AuthClosure,
    adjacency: MatrixAdjacency,
    /// Short id to event id; mtxdb's persisted closures are short-id bitmaps.
    labels: HashMap<u32, String>,
    /// Event id to short id, as a server holding short ids in its state maps has.
    shorts: HashMap<String, u32>,
}

impl Store {
    fn db(&self) -> &Database {
        self.db
            .as_ref()
            .expect("store database remains open during use")
    }
}

impl Drop for Store {
    /// Attempts to remove the benchmark directory, ignoring cleanup errors.
    fn drop(&mut self) {
        // Database owns open files under root; close it before directory cleanup.
        drop(self.db.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Replaces this process's `name`-qualified benchmark directory under the
/// system temporary directory with a database holding fixture events and auth
/// edges. Closures must be rebuilt separately; dropping the store attempts
/// to remove its directory.
///
/// # Panics
/// Panics if the fixture cites missing auth events, or database creation,
/// event writes, transaction commit, or auth-edge recording fails.
fn store_fixture(fixture: &Fixture, name: &str) -> Store {
    let root =
        std::env::temp_dir().join(format!("rezzy-bench-mtxdb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let db = Database::open(root.clone()).expect("open mtxdb");
    let adjacency = MatrixAdjacency::new(ShardType::Edges, ROOM);

    // Auth events first, so every event's targets are recorded before it.
    let order = rezzy::auth::roaring::AuthGraph::try_build(&fixture.events)
        .expect("fixture is auth-closed")
        .index
        .items()
        .to_vec();

    let txn = db.begin_transaction();
    for id in &order {
        let ev = &fixture.events[id];
        txn.put(
            ShardType::EventDag,
            EVENTS_COLLECTION,
            node_id(id),
            &NodeData::from_slice(pdu_json(ev).as_bytes()),
        )
        .expect("stage event");
    }
    txn.commit().expect("commit events");

    let mut labels = HashMap::with_capacity(order.len());
    let mut shorts = HashMap::with_capacity(order.len());
    for id in &order {
        let auth: Vec<&str> = fixture.events[id]
            .auth_events
            .iter()
            .map(String::as_str)
            .collect();
        let short = adjacency
            .record_event(&db, id, &[], &auth, None)
            .expect("record auth edges");
        labels.insert(short, id.clone());
        shorts.insert(id.clone(), short);
    }

    let closure = AuthClosure::from_adjacency(adjacency);
    Store {
        root,
        db: Some(db),
        closure,
        adjacency,
        labels,
        shorts,
    }
}

/// Reads events from mtxdb, parsing each once per provider.
///
/// `EventProvider` hands out references, so each event is parsed into a
/// per-provider `OnceCell`; a fresh provider per run starts cold.
struct MtxdbProvider<'a> {
    store: &'a Store,
    cells: HashMap<&'a str, OnceCell<Option<LeanEvent>>>,
    reads: AtomicUsize,
}

impl<'a> MtxdbProvider<'a> {
    /// Creates an empty per-event cache and read counter for the stored fixture.
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            cells: store
                .labels
                .values()
                .map(|id| (id.as_str(), OnceCell::new()))
                .collect(),
            reads: AtomicUsize::new(0),
        }
    }

    /// Reads and parses a stored event, counting the read even if no record
    /// exists. Missing records return `None`.
    ///
    /// # Panics
    /// Panics on storage errors or invalid stored JSON or event fields.
    fn read(&self, id: &str) -> Option<LeanEvent> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let record = self
            .store
            .db()
            .event_dag()
            .get(&EVENTS_COLLECTION, &node_id(id))
            .expect("read event")?;
        let value = JsonValue::parse_bytes(&record.bytes).expect("stored JSON parses");
        Some(LeanEvent::from_value(&value, Some("10")).expect("stored event parses"))
    }
}

impl EventProvider<String, JsonValue> for MtxdbProvider<'_> {
    /// Returns a cached event, reading it on first access. Unknown IDs and
    /// missing records return `None`; read and parse failures panic.
    fn get_event(&self, id: &String) -> Option<&LeanEvent> {
        self.cells
            .get(id.as_str())?
            .get_or_init(|| self.read(id))
            .as_ref()
    }
}

/// `∪ Cᵢ − ∩ Cᵢ` from mtxdb's persisted closures, each state event included.
/// An empty fork list returns an empty difference.
///
/// # Panics
/// Panics for an empty state map, missing short IDs, closures, or labels,
/// storage errors, or incompatible bitmap domains.
fn mtxdb_auth_difference(store: &Store, forks: &[StateMap]) -> Vec<String> {
    let mut union: Option<BitmapSet> = None;
    let mut intersection: Option<BitmapSet> = None;
    for state in forks {
        let mut chain: Option<BitmapSet> = None;
        for id in state.values() {
            let short = store
                .adjacency
                .short_id(&store.db(), id)
                .expect("read short id")
                .expect("state event is recorded");
            let mut closure = store
                .closure
                .get_at(&store.db(), short)
                .expect("read closure")
                .expect("closure is published");
            closure.insert(short);
            chain = Some(match chain {
                Some(c) => c.union(&closure).expect("same room domain"),
                None => closure,
            });
        }
        let chain = chain.expect("state map is not empty");
        union = Some(match union {
            Some(u) => u.union(&chain).expect("same room domain"),
            None => chain.clone(),
        });
        intersection = Some(match intersection {
            Some(i) => i.intersection(&chain).expect("same room domain"),
            None => chain,
        });
    }
    let (Some(union), Some(intersection)) = (union, intersection) else {
        return Vec::new();
    };
    union
        .difference(&intersection)
        .expect("same room domain")
        .iter()
        .map(|short| store.labels[&short].clone())
        .collect()
}

/// [`mtxdb_auth_difference`], cheaper: the unconflicted state's closures are
/// read once rather than once per fork (that part is in every `Cᵢ`, so it only
/// needs subtracting from the conflicted part), short ids come from memory,
/// and chains accumulate in place in a rezzy `Bitmap` because `BitmapSet`'s
/// set operations allocate a new set per merge.
///
/// # Panics
/// Panics for an empty fork list, missing short IDs, closures, or labels, or
/// closure-read errors.
fn mtxdb_auth_difference_merged(store: &Store, forks: &[StateMap]) -> Vec<String> {
    let closure_of = |id: &String, into: &mut rezzy::bitmap::Bitmap| {
        let short = store.shorts[id];
        into.insert(short);
        into.extend(
            store
                .closure
                .get_at(&store.db(), short)
                .expect("read closure")
                .expect("closure is published")
                .iter(),
        );
    };
    let first = &forks[0];
    let unconflicted = |key: &(rezzy::basespec::event_types::EventType, String), id: &String| {
        forks[1..].iter().all(|f| f.get(key) == Some(id))
    };

    let mut agreed = rezzy::bitmap::Bitmap::new();
    for (key, id) in first.iter() {
        if unconflicted(key, id) {
            closure_of(id, &mut agreed);
        }
    }
    let mut union = rezzy::bitmap::Bitmap::new();
    let mut intersection: Option<rezzy::bitmap::Bitmap> = None;
    for state in forks {
        let mut chain = rezzy::bitmap::Bitmap::new();
        for (key, id) in state.iter() {
            if !(first.get(key) == Some(id) && unconflicted(key, id)) {
                closure_of(id, &mut chain);
            }
        }
        union |= &chain;
        match &mut intersection {
            Some(both) => *both &= &chain,
            None => intersection = Some(chain),
        }
    }
    if let Some(both) = intersection {
        union -= &both;
    }
    union -= &agreed;
    union
        .iter()
        .map(|short| store.labels[&short].clone())
        .collect()
}

/// Resolves fixture forks with V2 and supplied auth-difference candidates,
/// returning the state size and the provider's cumulative read count.
///
/// # Panics
/// Propagates provider and lazy-resolver panics.
fn resolve_with_diff(
    provider: &MtxdbProvider<'_>,
    forks: &[StateMap],
    diff: impl IntoIterator<Item = String>,
) -> (usize, usize) {
    let resolved =
        rezzy::resolve_state_maps_lazy_with_diff(forks, provider, Some(diff), StateResVersion::V2);
    (resolved.len(), provider.reads.load(Ordering::Relaxed))
}

/// Warms up once, then reports mean milliseconds over `iterations` runs.
/// `run` returns the state size and read count; the last run's counts are shown.
fn time(label: &str, iterations: usize, mut run: impl FnMut() -> (usize, usize)) {
    run(); // warm up and verify the path completes
    let start = Instant::now();
    let mut result = (0, 0);
    for _ in 0..iterations {
        result = black_box(run());
    }
    let ms = start.elapsed().as_secs_f64() * 1_000.0 / iterations as f64;
    println!(
        "{label}: {ms:.3} ms/op, mtxdb reads/op={}, result entries={}",
        result.1, result.0
    );
}

/// Sums file sizes in bytes below `path`. Unreadable directories and metadata
/// contribute zero; unreadable directory entries are skipped.
fn dir_size(path: &std::path::Path) -> u64 {
    std::fs::read_dir(path).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => dir_size(&e.path()),
                _ => e.metadata().map_or(0, |m| m.len()),
            })
            .sum()
    })
}

/// Runs the `auth_difference` fixture against an mtxdb store.
pub fn run() {
    for (members, chain_len, iterations) in [(1_000, 32, 5), (10_000, 64, 3)] {
        println!(
            "\n--- auth_difference_mtxdb: {members} common members, {chain_len} events per branch ---"
        );
        let fixture = build_fixture(members, chain_len);

        let start = Instant::now();
        let store = store_fixture(&fixture, &members.to_string());
        let write_ms = start.elapsed().as_secs_f64() * 1_000.0;
        let start = Instant::now();
        let report = store
            .closure
            .rebuild(&store.db())
            .expect("rebuild closures");
        let rebuild_ms = start.elapsed().as_secs_f64() * 1_000.0;
        assert!(report.is_complete(), "fixture closures are complete");
        println!(
            "mtxdb write events + auth edges: {write_ms:.1} ms; closure rebuild: {rebuild_ms:.1} ms; \
             on disk: {:.1} KiB",
            dir_size(&store.root) as f64 / 1024.0
        );

        time("mtxdb/closure-diff-only", iterations, || {
            (mtxdb_auth_difference(&store, &fixture.forks).len(), 0)
        });
        let mut plain = mtxdb_auth_difference(&store, &fixture.forks);
        let mut merged = mtxdb_auth_difference_merged(&store, &fixture.forks);
        plain.sort();
        merged.sort();
        assert_eq!(plain, merged, "both mtxdb diffs agree");
        let mut from_graph = rezzy::auth::roaring::AuthGraph::try_build(&fixture.events)
            .expect("fixture is auth-closed")
            .state_auth_difference(&fixture.forks);
        from_graph.sort();
        assert_eq!(merged, from_graph, "mtxdb and rezzy AuthGraph diffs agree");
        println!(
            "diff paths agree: mtxdb closures, mtxdb merged, rezzy AuthGraph ({} events)",
            merged.len()
        );
        time("mtxdb/closure-diff-only merged", iterations, || {
            (
                mtxdb_auth_difference_merged(&store, &fixture.forks).len(),
                0,
            )
        });
        time("lazy/mtxdb-provider computed-diff", iterations, || {
            let provider = MtxdbProvider::new(&store);
            let resolved = rezzy::resolve_state_maps_lazy_with_diff(
                &fixture.forks,
                &provider,
                None::<Vec<String>>,
                StateResVersion::V2,
            );
            (resolved.len(), provider.reads.load(Ordering::Relaxed))
        });
        time("lazy/mtxdb-provider mtxdb-closure-diff", iterations, || {
            let provider = MtxdbProvider::new(&store);
            let diff = mtxdb_auth_difference_merged(&store, &fixture.forks);
            resolve_with_diff(&provider, &fixture.forks, diff)
        });

        // An in-memory rezzy AuthGraph kept beside the store (a server would
        // insert each event as it accepts it), with mtxdb serving the events.
        let graph = rezzy::auth::roaring::AuthGraph::try_build(&fixture.events)
            .expect("fixture is auth-closed");
        time(
            "lazy/mtxdb-provider rezzy-auth-graph-diff",
            iterations,
            || {
                let provider = MtxdbProvider::new(&store);
                let diff = graph
                    .hybrid_state_auth_difference(&fixture.forks, &provider)
                    .expect("graph is complete");
                resolve_with_diff(&provider, &fixture.forks, diff)
            },
        );
    }
}
