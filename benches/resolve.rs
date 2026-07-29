use std::collections::VecDeque;
use std::hint::black_box;
use std::mem::size_of;
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

struct DagFixture {
    graph: HashMap<String, LeanEvent<String>>,
    children: HashMap<String, Vec<String>>,
    ordered_ids: Vec<String>,
    chains: Vec<Vec<usize>>,
    layers: Vec<Vec<usize>>,
}

impl DagFixture {
    fn interleaved_chains(node_count: usize, chain_count: usize) -> Self {
        let mut graph = HashMap::with_capacity(node_count);
        let mut ordered_ids: Vec<String> = Vec::with_capacity(node_count);
        let mut chains = vec![Vec::new(); chain_count];
        let mut tails = vec![None::<String>; chain_count];

        while ordered_ids.len() < node_count {
            for chain_idx in 0..chain_count {
                if ordered_ids.len() >= node_count {
                    break;
                }
                let idx = ordered_ids.len();
                let event_id = format!("$c{chain_idx:02x}_{idx:08x}");
                let mut auth_events = Vec::with_capacity(1);

                if let Some(parent) = tails[chain_idx].clone() {
                    auth_events.push(parent);
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
                ordered_ids.push(event_id.clone());
                chains[chain_idx].push(idx);
                tails[chain_idx] = Some(event_id);
            }
        }

        let children = build_children_map(&graph);
        Self {
            graph,
            children,
            ordered_ids,
            chains,
            layers: Vec::new(),
        }
    }

    fn layered(layer_count: usize, layer_width: usize, fanout: usize) -> Self {
        let node_count = layer_count.saturating_mul(layer_width);
        let mut graph = HashMap::with_capacity(node_count);
        let mut ordered_ids: Vec<String> = Vec::with_capacity(node_count);
        let mut layers = Vec::with_capacity(layer_count);
        let mut previous_layer_ids: Vec<String> = Vec::new();

        for layer in 0..layer_count {
            let mut current_layer = Vec::with_capacity(layer_width);
            for pos in 0..layer_width {
                let idx = ordered_ids.len();
                let event_id = format!("$l{layer:04x}_{pos:08x}");
                let mut auth_events = Vec::with_capacity(fanout);

                if layer > 0 {
                    for parent_id in previous_layer_ids.iter().cycle().skip(pos).take(fanout) {
                        let parent_id = parent_id.clone();
                        if !auth_events.contains(&parent_id) {
                            auth_events.push(parent_id);
                        }
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
                ordered_ids.push(event_id.clone());
                current_layer.push(idx);
            }
            previous_layer_ids = current_layer
                .iter()
                .map(|&idx| ordered_ids[idx].clone())
                .collect();
            layers.push(current_layer);
        }

        let children = build_children_map(&graph);
        Self {
            graph,
            children,
            ordered_ids,
            chains: Vec::new(),
            layers,
        }
    }

    fn node_count(&self) -> usize {
        self.ordered_ids.len()
    }

    fn edge_count(&self) -> usize {
        self.graph
            .values()
            .map(|event| event.auth_events.len())
            .sum()
    }

    fn estimated_range_prefilter_bytes(&self) -> usize {
        let nodes = self.node_count();
        let edges = self.edge_count();
        nodes
            .saturating_mul(size_of::<Vec<u32>>())
            .saturating_add(edges.saturating_mul(size_of::<u32>()))
            .saturating_add(nodes.saturating_mul(size_of::<(u32, u32)>()))
            .saturating_add(nodes.saturating_mul(size_of::<u32>()))
    }
}

fn build_children_map(graph: &HashMap<String, LeanEvent<String>>) -> HashMap<String, Vec<String>> {
    let mut children: HashMap<String, Vec<String>> = HashMap::with_capacity(graph.len());
    for (id, event) in graph {
        for parent in &event.auth_events {
            if graph.contains_key(parent) {
                children.entry(parent.clone()).or_default().push(id.clone());
            }
        }
    }
    children
}

struct ReachabilityCase {
    label: &'static str,
    seeds: Vec<String>,
    candidates: Vec<String>,
    iterations: u32,
}

impl ReachabilityCase {
    fn by_indices(
        label: &'static str,
        fixture: &DagFixture,
        seeds: &[usize],
        candidates: &[usize],
    ) -> Self {
        Self {
            label,
            seeds: seeds
                .iter()
                .map(|&idx| fixture.ordered_ids[idx].clone())
                .collect(),
            candidates: candidates
                .iter()
                .map(|&idx| fixture.ordered_ids[idx].clone())
                .collect(),
            iterations: 3,
        }
    }
}

fn prefix_indices(indices: &[usize], count: usize) -> Vec<usize> {
    indices.iter().copied().take(count).collect()
}

fn slice_indices(indices: &[usize], start: usize, count: usize) -> Vec<usize> {
    indices.iter().copied().skip(start).take(count).collect()
}

fn benchmark_low_memory_case(
    fixture: &DagFixture,
    case: &ReachabilityCase,
) -> (Duration, Duration, Duration, usize) {
    let setup_start = Instant::now();
    let index = RangePrefilterReachability::build(&fixture.graph);
    let setup_elapsed = setup_start.elapsed();

    let index_start = Instant::now();
    let mut index_hits = Vec::new();
    for _ in 0..case.iterations {
        index_hits = index.filter_reachable(case.seeds.iter(), case.candidates.iter());
    }
    let index_elapsed = index_start.elapsed();

    let bfs_start = Instant::now();
    let mut bfs_hits = Vec::new();
    for _ in 0..case.iterations {
        bfs_hits = branchy_forward_reachable_bfs(&fixture.children, &case.seeds, &case.candidates);
    }
    let bfs_elapsed = bfs_start.elapsed();

    assert_eq!(index_hits, bfs_hits, "{} must match bfs", case.label);
    black_box((&index_hits, &bfs_hits));

    (setup_elapsed, index_elapsed, bfs_elapsed, index_hits.len())
}

fn cached_reachable_set(
    children: &HashMap<String, Vec<String>>,
    seeds: &[String],
) -> HashSet<String> {
    let mut reachable = HashSet::with_capacity(children.len().saturating_add(seeds.len()));
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

    reachable
}

fn benchmark_repeated_seed_cache(
    fixture: &DagFixture,
    seeds: &[String],
    candidate_batches: &[Vec<String>],
) -> (Duration, Duration, Duration, usize) {
    let setup_start = Instant::now();
    let cache = cached_reachable_set(&fixture.children, seeds);
    let setup_elapsed = setup_start.elapsed();

    let cached_start = Instant::now();
    let mut cached_hits = 0_usize;
    for batch in candidate_batches {
        cached_hits = cached_hits.saturating_add(
            batch
                .iter()
                .filter(|candidate| cache.contains(*candidate))
                .count(),
        );
    }
    let cached_elapsed = cached_start.elapsed();

    let direct_start = Instant::now();
    let mut direct_hits = 0_usize;
    for batch in candidate_batches {
        direct_hits = direct_hits
            .saturating_add(branchy_forward_reachable_bfs(&fixture.children, seeds, batch).len());
    }
    let direct_elapsed = direct_start.elapsed();

    assert_eq!(cached_hits, direct_hits);
    black_box((&cached_hits, &direct_hits));
    (setup_elapsed, cached_elapsed, direct_elapsed, cached_hits)
}

fn benchmark_candidate_sweep(fixture: &DagFixture, seeds: &[String], candidate_sizes: &[usize]) {
    let index = RangePrefilterReachability::build(&fixture.graph);
    for &candidate_size in candidate_sizes {
        let candidates: Vec<String> = fixture
            .ordered_ids
            .iter()
            .take(candidate_size)
            .cloned()
            .collect();

        let index_start = Instant::now();
        let index_hits = index.filter_reachable(seeds.iter(), candidates.iter());
        let index_elapsed = index_start.elapsed();

        let bfs_start = Instant::now();
        let bfs_hits = branchy_forward_reachable_bfs(&fixture.children, seeds, &candidates);
        let bfs_elapsed = bfs_start.elapsed();

        assert_eq!(index_hits, bfs_hits);
        black_box((&index_hits, &bfs_hits));
        report_comparison(
            &format!("resolve/candidate-sweep/{candidate_size}"),
            1,
            index_elapsed,
            1,
            bfs_elapsed,
        );
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

fn run_branchy_exact_suite() {
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
}

fn run_branchy_low_memory_suite() {
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

fn run_topology_query_matrix() {
    println!("\n--- topology/query matrix (range-prefilter vs prebuilt bfs) ---");
    for node_count in [25_000, 100_000] {
        let interleaved = DagFixture::interleaved_chains(node_count, 8);
        let chain0 = &interleaved.chains[0];
        let chain1 = &interleaved.chains[1];
        let mut no_hit = ReachabilityCase::by_indices(
            "interleaved/no-hit",
            &interleaved,
            &prefix_indices(chain0, 4),
            &prefix_indices(chain1, 256),
        );
        no_hit.iterations = if node_count <= 50_000 { 10 } else { 3 };
        let mut local_hit = ReachabilityCase::by_indices(
            "interleaved/local-hit",
            &interleaved,
            &slice_indices(&interleaved.chains[0], 64, 4),
            &slice_indices(&interleaved.chains[0], 60, 256),
        );
        local_hit.iterations = if node_count <= 50_000 { 10 } else { 3 };

        for case in [no_hit, local_hit] {
            let (setup_elapsed, index_elapsed, bfs_elapsed, hits) =
                benchmark_low_memory_case(&interleaved, &case);
            report_split(
                &format!("resolve/{}/{node_count} setup", case.label),
                setup_elapsed,
                index_elapsed,
            );
            report_comparison(
                &format!("resolve/{}/{node_count} hits={hits}", case.label),
                case.iterations,
                index_elapsed,
                case.iterations,
                bfs_elapsed,
            );
        }

        let layered = DagFixture::layered(200, 512, 4);
        let layer0 = &layered.layers[0];
        let mut shallow_hit = ReachabilityCase::by_indices(
            "layered/shallow-hit",
            &layered,
            &prefix_indices(layer0, 4),
            &prefix_indices(&layered.layers[1], 256),
        );
        shallow_hit.iterations = if node_count <= 50_000 { 10 } else { 3 };
        let mut deep_hit = ReachabilityCase::by_indices(
            "layered/deep-hit",
            &layered,
            &prefix_indices(layer0, 4),
            &prefix_indices(
                layered.layers.last().expect("layered graph has layers"),
                256,
            ),
        );
        let scattered_candidates: Vec<usize> = (0..layered.ordered_ids.len()).step_by(32).collect();
        let mut scattered = ReachabilityCase::by_indices(
            "layered/scattered",
            &layered,
            &prefix_indices(layer0, 4),
            &scattered_candidates,
        );
        let iters = if node_count <= 50_000 { 10 } else { 3 };
        deep_hit.iterations = iters;
        scattered.iterations = iters;

        for case in [shallow_hit, deep_hit, scattered] {
            let (setup_elapsed, index_elapsed, bfs_elapsed, hits) =
                benchmark_low_memory_case(&layered, &case);
            report_split(
                &format!("resolve/{}/{node_count} setup", case.label),
                setup_elapsed,
                index_elapsed,
            );
            report_comparison(
                &format!("resolve/{}/{node_count} hits={hits}", case.label),
                case.iterations,
                index_elapsed,
                case.iterations,
                bfs_elapsed,
            );
        }
    }
}

fn run_topology_stats_suite() {
    println!("\n--- topology size estimates ---");
    for (name, fixture) in [
        ("interleaved", DagFixture::interleaved_chains(25_000, 8)),
        ("layered", DagFixture::layered(200, 512, 4)),
    ] {
        println!(
            "resolve/topology/{name}: nodes={}, edges={}, approx_range_prefilter_bytes={}",
            fixture.node_count(),
            fixture.edge_count(),
            fixture.estimated_range_prefilter_bytes(),
        );
    }
}

fn run_repeated_seed_cache_suite() {
    println!("\n--- repeated-seed cache benchmark ---");
    let fixture = DagFixture::interleaved_chains(100_000, 8);
    let seeds = fixture
        .chains
        .first()
        .map(|chain| prefix_indices(chain, 8))
        .unwrap_or_default()
        .into_iter()
        .map(|idx| fixture.ordered_ids[idx].clone())
        .collect::<Vec<_>>();
    let candidate_batches = (0..100)
        .map(|offset| {
            fixture
                .ordered_ids
                .iter()
                .skip(offset)
                .step_by(32)
                .take(256)
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let (setup_elapsed, cached_elapsed, direct_elapsed, hits) =
        benchmark_repeated_seed_cache(&fixture, &seeds, &candidate_batches);
    report_split(
        "resolve/repeated-seed cache setup",
        setup_elapsed,
        cached_elapsed,
    );
    report_comparison(
        &format!("resolve/repeated-seed cache hits={hits}"),
        1,
        cached_elapsed,
        1,
        direct_elapsed,
    );
}

fn run_candidate_sweep_suite() {
    println!("\n--- candidate-size sweep ---");
    let fixture = DagFixture::layered(200, 512, 4);
    let seeds = fixture
        .layers
        .first()
        .map(|layer| prefix_indices(layer, 8))
        .unwrap_or_default()
        .into_iter()
        .map(|idx| fixture.ordered_ids[idx].clone())
        .collect::<Vec<_>>();
    benchmark_candidate_sweep(&fixture, &seeds, &[1, 4, 16, 64, 256, 1024]);
}

fn main() {
    run_branchy_exact_suite();
    run_branchy_low_memory_suite();
    run_topology_stats_suite();
    run_topology_query_matrix();
    run_repeated_seed_cache_suite();
    run_candidate_sweep_suite();
}
