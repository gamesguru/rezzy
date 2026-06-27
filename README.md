# Ruma-Lean: Matrix State Resolution Engine

Ruma-Lean is a high-performance, dependency-free Rust engine for Matrix State Resolution. It is a research model and reference implementation for Matrix state resolution versions 2, 2.1, and 2.1.1, designed for correctness and compliance.

## Features

- **Causal Domination Operator**: Safely resolves conflicting state in DAGs.
- **Lazy Projection**: Fast, memory-efficient state resolution by only loading required membership events.
- **Topological & Mainline Sorting**: Fast and robust DAG sorting to order events correctly.

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
