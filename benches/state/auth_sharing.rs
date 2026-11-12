//! Closure sharing: auth-graph build and bitmap difference.
//!
//! Two measurements:
//!
//! - `auth_graph_build`: building an [`AuthGraph`] whose children share auth
//!   tuples versus one where every child has a distinct tuple. The child count,
//!   closure depth and parent count are held constant; only the number of
//!   distinct tuples varies.
//! - `chunk_difference`: `(A ∪ B) − (A ∩ B)` on branches whose equal containers
//!   either share `Arc<Chunk>` pointers or were independently allocated by
//!   `decode(encode(..))`. This detects whether sharing affects merge cost.
//!
//! Run with:
//! `cargo +1.90.0 bench --manifest-path benches/Cargo.toml --bench rezzy -- auth_sharing`

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use rezzy::auth::roaring::AuthGraph;
use rezzy::bitmap::Bitmap;
use rezzy::LeanEvent;

use crate::common::alloc_probe;

fn event(id: &str, auth: &[&str]) -> LeanEvent {
    LeanEvent {
        event_id: id.to_owned(),
        auth_events: auth.iter().map(|id| (*id).to_owned()).collect(),
        ..Default::default()
    }
}

/// A graph with `children` leaf events and `tuples` distinct parent sets.
///
/// Four roots have empty auth. One anchor per tuple cites all four roots, so
/// every anchor closure is the same size. Each child cites three roots plus its
/// group's anchor, so closure size and depth are identical for every child;
/// only the number of distinct `auth_events` tuples changes.
fn graph(children: usize, tuples: usize) -> HashMap<String, LeanEvent> {
    let mut events = HashMap::with_capacity(children + tuples + 4);
    for root in 0..4 {
        let id = format!("$root{root}");
        events.insert(id.clone(), event(&id, &[]));
    }
    for tuple in 0..tuples {
        let id = format!("$anchor{tuple}");
        events.insert(
            id.clone(),
            event(&id, &["$root0", "$root1", "$root2", "$root3"]),
        );
    }
    for child in 0..children {
        let id = format!("$child{child}");
        let anchor = format!("$anchor{}", child % tuples);
        events.insert(
            id.clone(),
            event(&id, &[&anchor, "$root0", "$root1", "$root2"]),
        );
    }
    events
}

fn timed_build(events: &HashMap<String, LeanEvent>, iterations: u32) -> f64 {
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(AuthGraph::build(events));
    }
    start.elapsed().as_secs_f64() * 1e3 / f64::from(iterations)
}

fn run_build() {
    let children = 20_000usize;
    println!("\n--- auth_graph_build: {children} children, closure depth 2 ---");
    println!(
        "{:<10} {:>12} {:>14} {:>14}",
        "tuples", "build(ms)", "allocs", "alloc bytes"
    );

    for tuples in [1, children / 10, children / 2, children] {
        let events = graph(children, tuples.max(1));
        // Warm the allocator and CPU before measuring.
        black_box(AuthGraph::build(&events));

        let millis = timed_build(&events, 5);
        let (allocs, bytes) = alloc_probe::measure(|| {
            black_box(AuthGraph::build(&events));
        });
        println!("{tuples:<10} {millis:>12.3} {allocs:>14} {bytes:>14}");
    }
}

fn run_difference() {
    println!("\n--- chunk_difference: (A ∪ B) − (A ∩ B), one changed chunk per branch ---");
    println!(
        "{:<10} {:<12} {:>12} {:>14} {:>14}",
        "values", "chunks", "op(ms)", "allocs", "alloc bytes"
    );

    for values in [100_000u32, 1_000_000, 4_000_000] {
        let base: Bitmap = (0..values).map(|value| value.saturating_mul(2)).collect();
        let mut left = base.clone();
        left.insert(1);
        let mut right = base.clone();
        right.insert(3);

        // Equal contents, but every container is a fresh allocation.
        let left_independent = Bitmap::decode(&left.encode()).expect("decode left");
        let right_independent = Bitmap::decode(&right.encode()).expect("decode right");

        for (label, a, b) in [
            ("shared", &left, &right),
            ("independent", &left_independent, &right_independent),
        ] {
            // Warm up, then time the same three operations the auth difference
            // runs per branch: union, intersection, subtraction.
            let iterations = 10u32;
            for _ in 0..2 {
                let union = a | b;
                let intersection = a & b;
                black_box(union - intersection);
            }

            let start = Instant::now();
            for _ in 0..iterations {
                let union = a | b;
                let intersection = a & b;
                black_box(union - intersection);
            }
            let millis = start.elapsed().as_secs_f64() * 1e3 / f64::from(iterations);

            let (allocs, bytes) = alloc_probe::measure(|| {
                let union = a | b;
                let intersection = a & b;
                black_box(union - intersection);
            });

            println!(
                "{:<10} {:<12} {millis:>12.3} {allocs:>14} {bytes:>14}",
                values, label,
            );
        }
    }
}

pub fn run() {
    run_build();
    run_difference();
}
