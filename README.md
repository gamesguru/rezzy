# Ruma-Lean: Matrix State Resolution v2.1.1

Ruma-Lean is a formally verified, dependency-free Rust research engine for Matrix State Resolution. It serves as the reference implementation for the proposed **v2.1.1** standard, addressing critical topological anomalies and accumulator-retention defects found in production implementations (like Ruma v2.1 and Synapse).

_Note: ZK (Zero-Knowledge) acceleration experiments are currently paused to focus on formal verification and algorithmic stabilization of the core protocol._

## Architectural Innovations

Ruma-Lean introduces several mathematically rigorous solutions to state resolution, heavily leveraging applied graph theory and distributed systems security:

### 1. The Causal Domination Operator (CDO)

A vectorized topological filter executing on the Conflicted State Subgraph. By enforcing a strict partial order of administrative actions via a Bounded BFS algorithmic fix, CDO eliminates bypass windows like Phantom Join Rules and Mod Membership Evaporation.

### 2. $I_{\text{PL}}$ Fallback Context (Semantic vs. Syntactic)

Production engines often conflate **Syntactic Representation** (JSON payloads) with **Semantic Authority** (evaluated permissions), leading to strict schema panics when `m.room.power_levels` are redacted. Ruma-Lean decouples evaluation from validation: $I_{\text{PL}}$ is defined purely as an Evaluation-Time Closure parameterized by the immutable `m.room.create` event. It bypasses schema assertions while preserving total function safety.

### 3. $\mathcal{O}(1)$ Lazy Projection

To solve the accumulator-retention defect without the massive memory bottleneck of _Eager State Supplementation_, Ruma-Lean employs a Lazy Projection closure (`.or_else(|| unconflicted_state.get(&key))`). This $\mathcal{O}(1)$ memory/time logical union safely preloads unconflicted memberships into the initial auth overlay ($S_0$), completely neutralizing state reset attacks without scaling penalties.

### 4. Z3 SMT Verification

Instead of interactive game-theoretic models, safety is proven using a post-hoc Z3 SMT/CDCL topological framework. By universally quantifying over the unbounded space of all topologically valid partial orders (DAG configurations), the solver inherently proves deterministic safety against _any_ adversarial server collusion or network scheduler.

## Usage

```bash
# Run the core standard compliance test suite
cargo test

# Run the upstream runner against official Ruma integration tests
cargo test --test upstream_runner --features="mock-ruma"
```
