use std::collections::VecDeque;
use std::hint::black_box;
use std::time::{Duration, Instant};

use rezzy::{ForwardReachabilityIndex, HashMap, HashSet, LeanEvent, RangePrefilterReachability};

fn report_split(name: &str, setup: Duration, algo: Duration) {
    println!(
        "{name}: (setup: {:.6} ms, algo: {:.6} ms)",
        setup.as_secs_f64() * 1e3,
        algo.as_secs_f64() * 1e3
    );
}

fn report_comparison(
    name: &str,
    index_iterations: u32,
    index_elapsed: Duration,
    bfs_iterations: u32,
    bfs_elapsed: Duration,
) {
    let index_ms = index_elapsed.as_secs_f64() * 1e3;
    let bfs_ms = bfs_elapsed.as_secs_f64() * 1e3;
    let index_avg = index_ms / f64::from(index_iterations);
    let bfs_avg = bfs_ms / f64::from(bfs_iterations);
    let speedup = bfs_avg / index_avg;
    println!(
        "{name}: index={index_avg:.6} ms/query ({index_iterations} iters, {index_ms:.6} ms total), bfs={bfs_avg:.6} ms/query ({bfs_iterations} iters, {bfs_ms:.6} ms total), speedup={speedup:.2}x"
    );
}

struct Xorshift128 {
    state: [u64; 2],
}

impl Xorshift128 {
    fn new(seed: u64) -> Self {
        Self {
            state: [seed, seed ^ 0x9e37_79b9_7f4a_7c15],
        }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.state[0];
        let other = self.state[1];
        value ^= value << 23;
        value ^= value >> 17;
        value ^= other ^ (other >> 26);
        self.state = [other, value];
        value
    }
}

fn pick_branch_offset(generator: &mut Xorshift128) -> usize {
    usize::from(u8::try_from(generator.next() & 127).expect("benchmark offset fits u8"))
}

struct BranchyDag {
    graph: HashMap<String, LeanEvent<String>>,
    seeds: Vec<String>,
    candidates: Vec<String>,
}

impl BranchyDag {
    fn new(node_count: usize, seed_count: usize) -> Self {
        let mut generator = Xorshift128::new(0x6c62_5d1c_4d3b_a123);
        let mut graph = HashMap::with_capacity(node_count);
        let mut ordered_ids: Vec<String> = Vec::with_capacity(node_count);
        for idx in 0..node_count {
            let event_id = format!("$n{idx:08x}");
            let mut auth_events = Vec::with_capacity(3);

            if idx > 0 {
                let previous = idx.saturating_sub(1);
                auth_events.push(ordered_ids[previous].clone());
            }
            if idx > 2 {
                let back_offset = pick_branch_offset(&mut generator).saturating_add(1);
                let back = idx.saturating_sub(back_offset);
                let candidate = ordered_ids[back].clone();
                if !auth_events.contains(&candidate) {
                    auth_events.push(candidate);
                }
            }
            if idx > 8 && idx % 7 == 0 {
                let cross_offset = pick_branch_offset(&mut generator).saturating_add(1);
                let cross = idx.saturating_sub(cross_offset);
                let candidate = ordered_ids[cross].clone();
                if !auth_events.contains(&candidate) {
                    auth_events.push(candidate);
                }
            }

            graph.insert(
                event_id.clone(),
                LeanEvent {
                    event_id: event_id.clone(),
                    auth_events,
                    depth: u64::try_from(idx).expect("benchmark graph fits u64"),
                    ..Default::default()
                },
            );
            ordered_ids.push(event_id);
        }

        let seeds = ordered_ids[ordered_ids.len().saturating_sub(seed_count)..].to_vec();
        Self {
            graph,
            seeds,
            candidates: ordered_ids,
        }
    }

    fn children_by_parent(&self) -> HashMap<String, Vec<String>> {
        let mut children: HashMap<String, Vec<String>> = HashMap::with_capacity(self.graph.len());
        for (id, event) in &self.graph {
            for parent in &event.auth_events {
                if self.graph.contains_key(parent) {
                    children.entry(parent.clone()).or_default().push(id.clone());
                }
            }
        }
        children
    }
}

fn branchy_forward_reachable_bfs(
    children: &HashMap<String, Vec<String>>,
    seeds: &[String],
    candidates: &[String],
) -> Vec<usize> {
    let mut reachable = HashSet::with_capacity(children.len().saturating_add(candidates.len()));
    let mut queue = VecDeque::with_capacity(seeds.len());
    for seed in seeds {
        if reachable.insert(seed.clone()) {
            queue.push_back(seed.clone());
        }
    }

    while let Some(node) = queue.pop_front() {
        if let Some(children) = children.get(&node) {
            for child in children {
                if reachable.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
        }
    }

    candidates
        .iter()
        .enumerate()
        .filter_map(|(idx, candidate)| reachable.contains(candidate).then_some(idx))
        .collect()
}

fn benchmark_branchy_forward_reachability(
    node_count: usize,
    seed_count: usize,
) -> (Duration, Duration, Duration, u32, usize) {
    let setup_start = Instant::now();
    let branchy = BranchyDag::new(node_count, seed_count);
    let setup_elapsed = setup_start.elapsed();

    let index = ForwardReachabilityIndex::build(&branchy.graph);
    let iterations = if node_count <= 50_000 { 10 } else { 3 };
    let index_start = Instant::now();
    let mut index_hits = Vec::new();
    for _ in 0..iterations {
        index_hits = index.filter_reachable(branchy.seeds.iter(), branchy.candidates.iter());
    }
    let index_elapsed = index_start.elapsed();

    let children = branchy.children_by_parent();
    let bfs_start = Instant::now();
    let mut bfs_hits = Vec::new();
    for _ in 0..iterations {
        bfs_hits = branchy_forward_reachable_bfs(&children, &branchy.seeds, &branchy.candidates);
    }
    let bfs_elapsed = bfs_start.elapsed();

    assert_eq!(index_hits, bfs_hits);
    black_box((&index_hits, &bfs_hits));
    (
        setup_elapsed,
        index_elapsed,
        bfs_elapsed,
        iterations,
        index_hits.len(),
    )
}

fn benchmark_branchy_range_prefilter_reachability(
    node_count: usize,
    seed_count: usize,
) -> (Duration, Duration, Duration, u32, usize) {
    let setup_start = Instant::now();
    let branchy = BranchyDag::new(node_count, seed_count);
    let setup_elapsed = setup_start.elapsed();

    let index = RangePrefilterReachability::build(&branchy.graph);
    let iterations = if node_count <= 50_000 { 10 } else { 3 };
    let index_start = Instant::now();
    let mut index_hits = Vec::new();
    for _ in 0..iterations {
        index_hits = index.filter_reachable(branchy.seeds.iter(), branchy.candidates.iter());
    }
    let index_elapsed = index_start.elapsed();

    let children = branchy.children_by_parent();
    let bfs_start = Instant::now();
    let mut bfs_hits = Vec::new();
    for _ in 0..iterations {
        bfs_hits = branchy_forward_reachable_bfs(&children, &branchy.seeds, &branchy.candidates);
    }
    let bfs_elapsed = bfs_start.elapsed();

    assert_eq!(index_hits, bfs_hits);
    black_box((&index_hits, &bfs_hits));
    (
        setup_elapsed,
        index_elapsed,
        bfs_elapsed,
        iterations,
        index_hits.len(),
    )
}

fn main() {
    println!("\n--- branchy forward-reachability benchmark ---");
    for (node_count, seed_count) in [(25_000, 64), (100_000, 128), (250_000, 256)] {
        let (setup_elapsed, index_elapsed, bfs_elapsed, iterations, hits) =
            benchmark_branchy_forward_reachability(node_count, seed_count);
        report_split(
            &format!("resolve/reachability index setup/{node_count}"),
            setup_elapsed,
            index_elapsed,
        );
        report_comparison(
            &format!("resolve/reachability compare/{node_count} hits={hits}"),
            iterations,
            index_elapsed,
            iterations,
            bfs_elapsed,
        );
    }

    println!("\n--- branchy low-memory reachability benchmark ---");
    for (node_count, seed_count) in [(25_000, 64), (100_000, 128), (250_000, 256)] {
        let (setup_elapsed, index_elapsed, bfs_elapsed, iterations, hits) =
            benchmark_branchy_range_prefilter_reachability(node_count, seed_count);
        report_split(
            &format!("resolve/range-prefilter setup/{node_count}"),
            setup_elapsed,
            index_elapsed,
        );
        report_comparison(
            &format!("resolve/range-prefilter compare/{node_count} hits={hits}"),
            iterations,
            index_elapsed,
            iterations,
            bfs_elapsed,
        );
    }
}
