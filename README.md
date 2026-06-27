# Rezzy: Matrix State Resolution Engine

Rezzy is a high-performance, dependency-free Rust engine for Matrix State Resolution. It is a research model and reference implementation for Matrix state resolution versions 2, 2.1, and 2.1.1, designed for correctness and compliance.

## Features

- **Causal Domination Operator (CDO)**: Safely and optimally resolves conflicting state in DAGs.
- **Lazy Projection**: Fast, memory-efficient state resolution by only loading required membership events.
- **Topological & Mainline Sorting**: Fast and robust DAG sorting to order events correctly.
- **Pure Lattice-Coordinatized Projection**: Employs $O(1)$ Causal Coordinatization Projection and Commutative Join-Semilattice folding.

## Usage

### Build

```bash
cargo build --release
```

### Test

```bash
make test         # Run unit and integration tests
make rust/e2e     # Run E2E parity tests
```

### Test Coverage

To install and run test coverage via `cargo-tarpaulin`:

```bash
cargo install cargo-tarpaulin --features vendored-openssl
cargo tarpaulin
```

### Format & Lint

```bash
make format
make lint
```

## TODO

### Per-Event State Deltas (`resolve_with_deltas`)

Currently `resolve_lean` only outputs the final resolved state snapshot. For observability and debugging (e.g. visualizing state evolution through a fork), we need a variant that emits per-step deltas:

- Hook into the iterative auth-check loop and capture insertions/replacements at each step
- Emit a `Vec<StateDelta>` alongside the final `BTreeMap` result
- Each delta captures: event_id applied, (type, state_key) modified, old value evicted, new value inserted
- Useful for: timeline replay, state-res visualization, debugging ban/PL conflicts

### Checkpoint / Partial-Join Support

For simulating partial joins (e.g. federated rooms where a server doesn't have the full history):

- Allow starting resolution from a trusted state snapshot (checkpoint) as the unconflicted base
- Still require the full auth chain for any _conflicting_ events that diverge from the checkpoint
- Truncating the auth chain for conflicted events breaks topological ordering and can cause state resets (ref: CVE-2025-49090)
- API: accept an optional `checkpoint: BTreeMap<(String, Option<String>), String>` as the pre-resolved base state

### Auth Chain Subset Safety

Document and enforce the invariant: you can trust a snapshot for the unconflicted base, but the auth chain for conflicted events must be complete and uninterrupted. Partial auth chains lead to:

- Sorting failures (cannot establish mainline order)
- Auth check failures (missing historical power levels)
- Potential state reset attacks
