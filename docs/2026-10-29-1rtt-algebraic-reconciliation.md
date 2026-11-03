# 1-RTT Algebraic Set Reconciliation and Full-ID Recovery (2026-10-29)

Status: **analysis and architectural decision record.** Evaluates whether
algebraic set reconciliation can reconstruct full 256-bit UUID / event
identifiers in a single round trip without an intermediate second fetch or
collision buckets, and analyzes the trade-offs of column coupling versus
full-field widening.

---

## Executive Summary

Reconciling 256-bit identifiers via four parallel, independently reconciled
64-bit quartile columns **cannot recover 256-bit rows**. Algebraic set
reconciliation is linear and commutative: it sees each element only through a
key and erases order, adjacency, and row position. Four independently reconciled
columns decode to four _unordered sets_, and joining them back into 256-bit
tuples is underdetermined ($(d!)^3$ possible permutations).

The current second fetch in MSC4521 / `rezzy-recon` buys the 192 bits of
information that a 64-bit sketch does not carry. To eliminate this second fetch,
those bits—or a correlated algebraic equivalent—must be transmitted in the
initial response. This requires roughly **$4\times$ the sketch bytes**
($C \times 32$ bytes instead of $C \times 8$ bytes for capacity $C$), matching
the information-theoretic lower bound for 1-RTT reconstruction.

Two viable architectures exist for 1-RTT reconstruction:

1. **Option A (Full 256-bit element in $\mathrm{GF}(2^{256})$):** Strictly
   eliminates collision buckets and phase-2 fetches. However, it incurs a
   $5\text{–}13\times$ increase in root-finding work units plus a multi-limb
   field multiplication penalty.
2. **Option B2 (128-bit locator + 128-bit correlated payload in
   $\mathrm{GF}(2^{128})$):** Reuses existing digest splits (`h128`), leverages
   `NodeSummary.digest` as the $V_0$ moment, and avoids degree-256 root finding
   via cheap $O(d^2)$ Forney solves. It is **collision-resistant to a $2^{64}$
   adversarial birthday bound**, but structurally retains a rare fallback path.

For Matrix / `rezzy` DAG replication specifically, **1-RTT ID resolution does
not achieve 1-RTT event replication**, because full event JSON payloads
($1\text{–}3\text{ KiB}$) must still be fetched in a subsequent request. The
two-phase model remains optimal for event stores.

### Implementation status (2026-11-03)

The repository now contains a concrete GF(2^256) polynomial-basis field and a
generic PinSketch decoder. Its fixed reduction polynomial is
`x^256 + x^10 + x^5 + x^2 + 1`; Rabin irreducibility and a reducible-polynomial
sanity check live in `rezzy-recon/src/binary_field.rs`. The decoder's 300M
work-unit ceiling is a practical limit, not a proven worst-case bound. Release
probes observed 232.6M units for 256 consecutive roots and 36.1M for 256
pseudorandom roots. Other adversarial factor shapes may exhaust the ceiling.

The runnable release benchmark on the current host measured about 21 ns for
multiplication and 16 ns for square; cached decode took 2.59 s for 256
consecutive roots and 0.86 s for 256 pseudorandom roots. Earlier isolated
probes measured 45 ns and 39 ns for multiply and square, showing that these
microbenchmarks are host- and harness-sensitive. `Gf256::square` is a
dedicated bit-spreading implementation followed by polynomial reduction; it
does not fall back to multiplication. The full wide-field reconciliation
profile is not yet wired into the existing MSC4521 `ElementHash` and
bucket-exchange path.

---

## 1. Why "Ordered" or "Adjacent" Lists Do Not Fix Pairing

The core primitive of PinSketch (`SyndromeSketch::toggle` in
`rezzy-recon/src/algebraic.rs`) is:

```rust
let squared = gf64_mul(value, value);
let mut odd_power = value;
for coordinate in &mut self.coordinates {
    *coordinate ^= odd_power;
    odd_power = gf64_mul(odd_power, squared);
}
```

This operation is XOR-commutative. Inserting elements in any order produces
identical syndromes; the wire result is invariant under permutation. The decoder
returns roots as an unordered set (`BucketDecodeSuccess.roots: Vec<u64>`), which
verification subsequently sorts. There is no adjacency channel in the sketch.

If four independent sketches are executed over the quartiles
$(q_0, q_1, q_2, q_3)$:

- The decoder yields four unordered sets $S_0, S_1, S_2, S_3$.
- Nothing indicates which $q_1$ belongs to which $q_0$.
- Sorting each set independently does not help: numerical sorting on $q_0$
  orders elements by their $q_0$ bits, which is completely uncorrelated with the
  numerical sort order of their $q_1$ bits. _Counterexample:_ Let $A = (1, 4)$
  and $B = (2, 3)$. Sorted $q_0$ yields $(A, B)$; sorted $q_1$ yields $(B, A)$.
  Independent column orderings do not form a common row ordering.
- Serializing columns adjacent in memory changes local byte layout, not the
  algebra: the sketch only observes the per-element key fed into the
  accumulator.

### Proof: Post-Processing / Convolution Cannot Recover Pairing

Suppose two sets of 128-bit items $(a, b)$ differ:

- $\text{Set}_1 = \{(A_1, B_1), \; (A_2, B_2)\}$
- $\text{Set}_2 = \{(A_1, B_2), \; (A_2, B_1)\}$

Their independent power-sum syndrome vectors are:
$$S_k^{(A)}(\text{Set}_1) = A_1^k \oplus A_2^k = S_k^{(A)}(\text{Set}_2)$$
$$S_k^{(B)}(\text{Set}_1) = B_1^k \oplus B_2^k = S_k^{(B)}(\text{Set}_2)$$

Because the syndrome vectors are identical, any deterministic post-processing
verb (concatenation, convolution, Fourier transform) produces identical outputs
for both sets. In the time domain, convolving $S^{(A)} * S^{(B)}$ mixes all
pairwise products $A_i \cdot B_j$ for all $i, j$, failing to isolate the
diagonal $i = j$. The pairing information is discarded at encode time.

---

## 2. Characteristic-2 Cancellation and Locator Collisions

To preserve pairing algebraically, the payload must be coupled to the locator
**at encode time** via cross-moments:
$$S_k^{(0)} = \sum_{x \in \mathcal{S}} q_{0, x}^k \quad (\text{odd } k)$$

$$
S_k^{(m)} = \sum_{x \in \mathcal{S}} q_{m, x} \cdot q_{0, x}^k
\quad (m \in \{1, 2, 3\}, \; k = 0 \dots C-1)
$$

### The Threat Model: Cancellation vs. Recovery

If two distinct elements in the symmetric difference share the same locator $r$:
$$X = (r, a, b, c), \quad Y = (r, a', b', c')$$

In characteristic 2: $$S_k^{(0)} \text{ contribution} = r^k \oplus r^k = 0$$
$$S_k^{(m)} \text{ contribution} = (a \oplus a') \cdot r^k$$

The consequences depend on whether the collision is opposite-sided or
same-sided:

- **Opposite-sided collision (one local-only, one remote-only):** The client
  possesses its local payload $a$ and can compute the remote payload as
  $a' = (a \oplus a') \oplus a$.
- **Same-sided collision (two remote-only elements):** This is the adversarial
  case. The client knows neither payload. Because $r$ cancelled out of $S_0$,
  Berlekamp–Massey never outputs $r$ as a locator. Even if $r$ is deduced from
  the ratio $S_1[1] / S_1[0]$, the payload array only provides $a \oplus a'$,
  yielding one equation for two unknowns.

**Conclusion:** A locator collision produces an underdetermined system when both
elements are on the side being reconstructed. A Vandermonde rank check provides
**collision detection, not collision recovery**. A fallback path (bucket
splitting or direct fetch) remains structurally necessary whenever the locator
is not injective on the ID space.

---

## 3. Architecture Comparison

### Option A: Full 256-bit Elements in $\mathrm{GF}(2^{256})$ (Strict No-Collision-Fallback)

The entire 32-byte digest is treated as an element of $\mathrm{GF}(2^{256})$.

- **Topology:** Bucket placement and resident strata stay at 64 bits (`h64`).
  Only the bucket sketch element is widened to 256 bits.
- **Semantics:** Decoded roots _are_ the full IDs. There are no locator
  collisions ($x \oplus x = 0$ only if the full 256-bit digests match).
  `verify_follow_up`, `m_roots`, and `SortedPopulation::candidates` disappear
  entirely.
- **Zero-Element Invariant:** PinSketch requires non-zero elements ($x \ne 0$).
  Reject the all-zero 32-byte digest at insertion. Do not alias it to `1` or use
  an affine offset that leaves one digest unrepresentable. The placement key may
  be zero; only the full field element must be nonzero.
- **Compute Cost:** Using the pinned work model
  $\text{ceiling}(d) = (2F+8)d^2 + F(F+8)d$:

| Degree ($d$)       | $F = 64$ Work Units | $F = 256$ Work Units | Work-Unit Ratio |
| :----------------- | :------------------ | :------------------- | :-------------- |
| **8** (stratum)    | 45,568              | 573,952              | **12.6×**       |
| **32** (bucket)    | 286,720             | 2,695,168            | **9.4×**        |
| **256** (overflow) | 10,092,544          | 51,380,224           | **5.1×**        |

This ratio applies _before_ accounting for multi-limb multiplication overhead (4
limbs vs. native 64-bit PCLMULQDQ / AVX-512). `MAX_BATCH_FACTOR_WORK` would
increase from $\sim 36.7\text{M}$ to $\sim 345\text{M}$.

### Option B: Correlated Payload Moments (Interleaved Reed-Solomon)

Send a primary locator sketch plus coupled payload moments. Primary decode finds
roots $k_i$; payloads are extracted via Forney's formula in $O(d^2)$ total field
operations:
$$\Omega_c(z) \equiv V_c(z) \Lambda(z) \pmod{z^d}, \quad v_{c, i} = \frac{\Omega_c(k_i^{-1})}{\Lambda'(k_i^{-1})}$$

Because cross-moments do not benefit from Frobenius squaring shortcuts
($v k^{2j} \ne (v k^j)^2$), the responder must transmit $C$ coordinates
($j = 0 \dots C-1$) for each payload column.

| Scheme                | Locator Key  | Payload     | Wire / Coord           | Adversarial Collision Floor                                                                         | Decode Complexity                              |
| :-------------------- | :----------- | :---------- | :--------------------- | :-------------------------------------------------------------------------------------------------- | :--------------------------------------------- |
| **Current (MSC4521)** | 64-bit `h64` | —           | $8\text{ B}$           | $2^{32}$ birthday                                                                                   | $\mathrm{GF}(2^{64})$ BM + 2nd RTT             |
| **B1**                | 64-bit       | 192-bit     | $32\text{ B}$          | $2^{32}$ birthday                                                                                   | $\mathrm{GF}(2^{64})$ BM + 3 Forney solves     |
| **B2**                | **128-bit**  | **128-bit** | **$\sim 32\text{ B}$** | **$2^{64}$ birthday**                                                                               | **$\mathrm{GF}(2^{128})$ BM + 1 Forney solve** |
| **Option A**          | 256-bit      | —           | $32\text{ B}$          | No locator collision; residual identity collision is SHA-256's ~`2^128` birthday / `2^256` targeted | $\mathrm{GF}(2^{256})$ BM                      |

### Why B2 is the Practical Sweet Spot for 1-RTT

If paying $\sim 4\times$ wire is acceptable:

1. **Reuses Existing Digest Slices:** Locator is digest bytes `0..16`; payload
   is bytes `16..32` (matching the existing `h128` residual).
2. **Wire Savings via `V_0` Reuse:** The $j = 0$ payload moment is $\sum v_i$,
   which is _already transmitted_ as `NodeSummary.digest`. The wire needs only
   $C-1$ payload coordinates, taking $16C + 16(C-1) = 32C - 16$ bytes per
   bucket.
3. **Avoids the $\mathrm{GF}(2^{256})$ Root-Finding Cliff:** Root finding stays
   in $\mathrm{GF}(2^{128})$, while payload extraction is a fast $O(d^2)$ solve.
4. **Collision Boundary:** Pushes the adversarial birthday floor to $2^{64}$,
   rendering attacks infeasible without ASIC-scale investment, while retaining
   an explicit fallback path for correctness.

---

## 4. Resident Strata and On-Demand Extraction Costs

Keeping the resident strata estimator at $\mathrm{GF}(2^{64})$ while using
Option A or B2 for bucket sketches preserves the compact **2 KiB resident
state** in `ResidentKernel`.

However, on-demand payload evaluation is not zero-cost:

- **Responder Extraction:** Non-empty bucket extraction requires computing
  cross-moments for all population items in the bucket ($\sim 4\times$
  polynomial MAC operations).
- **Wire Size:** The bucket sketch wire footprint is $4\times$ larger
  ($1024\text{ B}$ vs $256\text{ B}$ at $C=32$).
- **Steady-State ($d = 0$):** Reconciliations where the root summaries match
  short-circuit before any bucket sketch is requested, paying zero wire or
  extraction penalty.
- **Accounting:** The work budget (`MAX_BUCKETED_SKETCH_CAPACITY` and per-bucket
  work ceilings) must be updated to account for Forney evaluation and multi-limb
  MACs.

---

## 5. The "1-RTT Mirage" in Event / DAG Replication

In decentralized event replication (Matrix / `rezzy` DAGs), **reconstructing
256-bit IDs in 1-RTT does not achieve 1-RTT event synchronization**:

- The receiver requires the event JSON, auth chain, signatures, and state
  relations ($1\text{–}3\text{ KiB}$ per event).
- After learning the 256-bit IDs in Round 1, the client must still dispatch an
  HTTP request to fetch the event bodies.
- Before: 2 RTT if phase 2 batches ID resolution with body fetch; otherwise 3
  RTT (sketch → IDs → bodies).
- After Option A: 2 RTT (sketch → bodies). Never 1 RTT for event replication.

Unless the reconciliation sketch carries the event bodies themselves
(prohibitive for algebraic sketches), eliminating Phase 2 ID resolution saves at
most one round trip—zero if IDs and bodies are already batched—and never reaches
1 RTT for event replication.

---

## 6. Decision Matrix

| Consumer / Workload                             | Recommendation                                    | Rationale                                                                                                                                 |
| :---------------------------------------------- | :------------------------------------------------ | :---------------------------------------------------------------------------------------------------------------------------------------- |
| **Event / DAG Replication** (`rezzy`)           | **Retain Two-Phase MSC4521**                      | Body fetch dominates total time; avoids $4\times$ wire and extraction penalty on sketches.                                                |
| **ID-Only Sync (Strict No-Collision-Fallback)** | **Option A ($\mathrm{GF}(2^{256})$)**             | No locator collisions; guarantees one RTT when $d \le C$, with no collision-resolution fetch. Capacity overflow can still require splits. |
| **ID-Only Sync (Practical)**                    | **Option B2 ($128/128$, $\mathrm{GF}(2^{128})$)** | Reuses `h128` residual as $V_0$; $2^{64}$ adversarial floor; avoids degree-256 root finding.                                              |
| **GF($2^{64}$)-Only Hardware Constraint**       | **Option B1 ($64/192$, $\mathrm{GF}(2^{64})$)**   | Preserves existing SIMD paths; must document $2^{32}$ adversarial collision fallback.                                                     |

---

## 7. Key Takeaways

1. **Independent quartile sketches cannot be post-processed into rows.** Order
   and pairing are destroyed at encode time.
2. **Coupling columns via cross-moments ($S_m[k] = \sum q_m \cdot q_0^k$)
   recovers rows in 1-RTT.**
3. **Locator collisions in characteristic 2 cause cancellation, not recovery.**
   When both colliding elements are remote-only, individual payloads cannot be
   separated. Detection is not correction.
4. **B2 is collision-resistant to $2^{64}$, not collision-free.** Only Option A
   eliminates **locator-collision** fallbacks entirely.
5. **For event replication, 1-RTT ID recovery is an illusion.** The payload
   fetch remains the true latency bottleneck.

---

## 8. Option A: consolidated ledger (supersedes earlier tables)

Option A = GF(2^256) bucket sketches, GF(2^64) strata, fixed digest windows. It
requires a **new profile/version**: fixed windows change `h64` from "first
non-zero chunk" to `digest[0..8]`, which is incompatible with MSC4521 (Option A
already breaks compatibility; make it explicit).

### 8.1 Digest windows

| Window           | Role                                                        |
| :--------------- | :---------------------------------------------------------- |
| `digest[0..8]`   | bucket placement key (zero is legal)                        |
| `digest[8..16]`  | strata key (`0 -> 1` alias kept; affects only the estimate) |
| `digest[16..32]` | `h128` residual / `NodeSummary`                             |
| `digest[0..32]`  | sketch element (full field element)                         |

Reject only the all-zero full digest at insert (no `0 -> 1` aliasing on the full
element). Drop the `h64 == 0` guard from `ResidentKernel::insert`; bucket
placement may use a zero prefix. Keep the zero-element rejection in
`SyndromeSketch::toggle`, but apply it to the full 32-byte digest. The current
first-non-zero scan makes `h64 | q3 | h128` disjoint only probabilistically;
fixed windows make it strict. `FixedWindowElementHash` now represents this
mapping explicitly, separately from legacy `ElementHash`; using it in
reconciliation still requires the new profile's zero-element and wire rules.

### 8.2 Collision and security surfaces

| Surface                                         | Before (MSC4521, GF(2^64))                                                                      | After (Option A)                                                                                                                                                                                                                               |
| :---------------------------------------------- | :---------------------------------------------------------------------------------------------- | :--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Locator collision (L/M, opposite-sided)         | `h64` pair (~2^32 birthday); recoverable in phase 2 via `h64 -> h128` map, subject to ambiguity | No locator collision                                                                                                                                                                                                                           |
| Locator collision (M/M or L/L, same-sided)      | Cancels in sketch; **not** recoverable by phase 2; falls back                                   | No locator collision                                                                                                                                                                                                                           |
| Residual identity collision                     | SHA-256, same                                                                                   | SHA-256: ~2^128 birthday (free pair), ~2^256 targeted second preimage. Recalculate if IDs are not SHA-256 outputs                                                                                                                              |
| Accumulator forgery, accumulator-only fast path | ~2^37 (Wagner over 128-bit XOR)                                                                 | Width unchanged; the ~2^37 estimate may still apply                                                                                                                                                                                            |
| Accumulator forgery, sketch-gated               | Gated on `h64`-colliding pairs (~2^32)                                                          | Pair must be invisible to the full-ID sketch, so dominated by SHA-256 identity collisions. The old 2^37 does not carry over and is **not yet re-derived**. Holds only if the accumulator is a hint, not a trust gate that skips reconciliation |
| Prefix grinding (placement DoS)                 | ~2^depth to target a prefix; no-amplification policy                                            | Unchanged targeting cost. Wider decode raises defender cost per poisoned element, so the policy may need retuning                                                                                                                              |
| LtHash anchoring                                | not wired into recon                                                                            | ~2^97 parameter-dependent estimate, not a theorem; needs an authenticated out-of-band anchor. Orthogonal to the locator width                                                                                                                  |

### 8.3 Pinned GF(2^256) field profile

The committed wide-field decoder pins the polynomial basis
`x^256 + x^10 + x^5 + x^2 + 1`. Elements are four little-endian `u64`
limbs, bit `i` is the coefficient of `x^i`, and the byte mapping is
little-endian. The reduction polynomial is justified by Rabin's
irreducibility test in the `binary_field` test module, next to a
reducible-polynomial negative control.

`MAX_FACTOR_WORK_GF256` is 300,000,000 work units. Release measurements
for degree-256 inputs were 232,591,912 units for consecutive roots and
36,127,278 for pseudorandom roots. This is a practical ceiling, not a
proven worst case: the work model's full binary recursion bound is
5,148,287,480 units (a chain of `(1, d-1)` splits maximizes the model),
and constructing an input that reaches it is still open. Exhaustion
returns `BudgetExhausted` and falls back to the non-algebraic path.

### 8.4 Cost, latency and memory

| Metric                          | Before                                                                                                                       | After                                                                                  |
| :------------------------------ | :--------------------------------------------------------------------------------------------------------------------------- | :------------------------------------------------------------------------------------- |
| ID-only sync latency            | 2 RTT                                                                                                                        | 1 RTT (one fewer RTT; not necessarily a 50% wall-clock drop)                           |
| Event replication latency       | 2 RTT if ID and body are batched in phase 2; otherwise 3 RTT (sketch, IDs, bodies)                                           | 2 RTT (sketch, bodies); body fetch remains                                             |
| Post-decode verification        | O(d·C) re-encode, O(d) count/residual, local classification (O(d log N) or amortized O(d)), plus candidate/ambiguity lookups | Same, minus the candidate map and ambiguity branches. Not O(1)                         |
| Per-op field multiply           | ~1 base clmul + reduction                                                                                                    | 9 base clmuls via Karatsuba + reduction; ~22-29 ns/op in release probes (host-specific) |
| Per-op field square             | No dedicated wide-field operation                                                                                            | Bit spreading + reduction; ~16-18 ns/op in release probes (host-specific)               |
| Root finding work units         | 16,000,000 ceiling                                                                                                           | 300,000,000 practical ceiling; measured 232.6M consecutive, 36.1M random at d=256      |
| Full decode wall clock          | —                                                                                                                            | ~2.8 s consecutive, ~0.9 s random at d=256 (release host)                              |
| Wire per bucket sketch (C = 32) | 256 B                                                                                                                        | 1024 B                                                                                 |
| Resident strata                 | 2048 B, F = 64                                                                                                               | unchanged                                                                              |

| Constant                 | Today      | Option A                                                              |
| :----------------------- | :--------- | :-------------------------------------------------------------------- |
| `MAX_FACTOR_WORK`        | 16,000,000 | 16,000,000 (GF(2^64) strata)                                          |
| `MAX_FACTOR_WORK_GF256`  | n/a        | 300,000,000 practical; 5,148,287,480 model worst-case                 |
| `MAX_BATCH_FACTOR_WORK`  | 36,700,160 | ~344,981,504 (128 x ceiling(32, F=256))                               |
| `MAX_STRATA_FACTOR_WORK` | 1,458,176  | 1,458,176 (F=64, d=8)                                                 |

Population memory (today `SortedPopulation` = `Vec<u64>` + `Vec<u128>` = 24
B/elem):

| Layout                                        | B/elem | Delta | At 10M events |
| :-------------------------------------------- | :----- | :---- | :------------ |
| Full digest only, derive windows              | 32     | +8    | +80 MB        |
| Digest + separate 8 B placement key           | 40     | +16   | +160 MB       |
| Digest + placement + separate residual column | 56     | +32   | +320 MB       |

### 8.5 Implementation checklist

1. Genericize `pinsketch` over a field trait or const generic, not just the cost
   functions: `FACTOR_TRIALS`, `TRACE_SQUARES`, `frobenius_basis_cost`,
   `factor_trial_cost_with_basis`, `solve_quadratic_form` and the hardcoded
   `MAX_FACTOR_WORK` all depend on `FIELD_BITS`. Use F = 256 for bucket sketches
   and F = 64 for strata; do not set one global `FIELD_BITS = 256`. **Decoder
   genericization and the GF(2^256) field profile are implemented.** The
   production full-ID sketch, bucket extraction and verification wiring remain
   pending.
2. Keep prefix bisection, capacity overflow, budgets, residual/count checks and
   the prefix-grinding policy. Delete `SortedPopulation::candidates`, ambiguity
   branches in `verify_decode`, collision-repeat tracking and phase-2
   `verify_follow_up` (fold its checks into local verification).
3. Strata key stays `digest[8..16]`; never derive it from `h128`. Fixed-window
   digest representation is implemented; use in the versioned reconciliation
   path remains pending.
4. **Benchmarks:** a runnable suite entry covers GF(2^256) multiply, square,
   full-ID set sketch construction, and cached decode at several degrees. Full-ID
   extraction/exchange is pending production wiring. `benches/bench-results.csv`
   remains GF(2^64)-only.
5. DX sketch: O(N) fingerprint filter; does not recover 256-bit IDs.
   Experimental comparison in `filter_spillover` only, not `triage`.
