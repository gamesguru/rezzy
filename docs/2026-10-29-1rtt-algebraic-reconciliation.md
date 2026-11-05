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
the leading $C \times 256$-bit term in the information-theoretic lower bound for
recovering up to $C$ arbitrary 256-bit elements. The exact bound has lower-order
terms, including uncertainty in the element count.

Two viable architectures exist for 1-RTT reconstruction:

1. **Option A (Full 256-bit element in $\mathrm{GF}(2^{256})$):** Eliminates
   truncation-induced locator collisions and phase-2 ID lookup for distinct
   digests. SHA-256 identity collisions remain possible. It incurs a
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

The runnable release benchmark on the current host measured about 22 ns for
multiplication and 16 ns for square; cached decode took 2.76 s for 256
consecutive roots and 0.90 s for 256 pseudorandom roots. Earlier isolated probes
measured 45 ns and 39 ns for multiply and square, then 29 ns and 18 ns after the
reduce/square rewrite, showing that these microbenchmarks are host- and
harness-sensitive. `Gf256::square` is a dedicated bit-spreading implementation
followed by polynomial reduction; it does not fall back to multiplication. The
GF(2^256) client selection (`select_gf256_action`), exchange
(`Gf256BucketExchange`), server node builders and summary-aware batch decoder
are implemented and exercised end to end through the public API. New callers
select this path by default through `select_default_action`, which returns the
GF(2^256) action. The existing path (`select_action`, `ElementHash`,
`SyndromeSketch`, phase-2 verification) remains available for callers that
explicitly stay on GF(2^64); nothing is deleted or renamed. The strata estimator
stays GF(2^64) for both. What is still missing is transport wiring for the
GF(2^256) payload and advertising it in a wire capability field.

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

### Option A: Full 256-bit Elements in $\mathrm{GF}(2^{256})$ (No Locator-Collision Fallback)

The entire 32-byte digest is treated as an element of $\mathrm{GF}(2^{256})$.

- **Topology:** Bucket placement and resident strata stay at 64 bits (`h64`).
  Only the bucket sketch element is widened to 256 bits.
- **Semantics:** Decoded roots _are_ the full IDs. There are no locator
  collisions ($x \oplus x = 0$ only if the full 256-bit digests match). This
  path does not need `verify_follow_up`, `m_roots`, or
  `SortedPopulation::candidates`; those remain in the existing GF(2^64) path.
- **Zero-Element Invariant:** PinSketch requires non-zero elements ($x \ne 0$).
  Treat the all-zero 32-byte digest as one reserved sentinel: inserting it into
  the algebraic population reports `ZeroSketchElement`, and the caller covers it
  through an explicit non-algebraic fallback (for example, direct ID fetch for
  that event or a direct event-ID difference for the room) so it is still
  reconciled exactly. Do not alias it to `1` or use an affine offset that leaves
  another digest unrepresentable. The placement key may be zero; only the full
  field element must be nonzero. This is a single reserved sentinel with its own
  exact fallback, not a locator-collision fallback for other digests.
- **Compute Cost:** Using the pinned work model
  $\text{ceiling}(d) = (2F+8)d^2 + F(F+8)d$:

| Degree ($d$)       | $F = 64$ Work Units | $F = 256$ Work Units | Work-Unit Ratio |
| :----------------- | :------------------ | :------------------- | :-------------- |
| **8** (stratum)    | 45,568              | 573,952              | **12.6×**       |
| **32** (bucket)    | 286,720             | 2,695,168            | **9.4×**        |
| **256** (overflow) | 10,092,544          | 51,380,224           | **5.1×**        |

This ratio applies _before_ accounting for multi-limb multiplication overhead (4
limbs vs. native 64-bit PCLMULQDQ / AVX-512). `MAX_BATCH_FACTOR_WORK` would
increase from $\sim 36.7\text{M}$ to $\sim 345\text{M}$. The $d = 256$ entry is
a single-call model estimate: actual consumption depends heavily on root
structure (Section 8.3 reports 36,127,278 units for pseudorandom roots and
232,591,912 units for consecutive roots). Thus, the formula values represent
single-call model estimates rather than universal lower bounds across all
workloads, and cannot serve as direct budget calibrations.

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
5. **Zero-Locator Invariant:** Option B2 requires nonzero locator keys
   ($k_i \ne 0$) because root zero is rejected by the algebraic decoder and
   cannot be evaluated in Forney's formula ($k_i^{-1}$ is undefined). If a
   digest's 16 locator bytes (`digest[0..16]`) are all zero, route that
   identifier through the non-algebraic fallback; do not silently omit it or
   alias the locator. This is distinct from Option A, where fallback is needed
   only for the all-zero 32-byte digest (`digest[0..32] == 0`), while zero
   prefixes in `digest[0..16]` remain valid field elements.

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

| Consumer / Workload                              | Recommendation                                    | Rationale                                                                                                                                                                                                          |
| :----------------------------------------------- | :------------------------------------------------ | :----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Event / DAG Replication** (`rezzy`)            | **Retain Two-Phase MSC4521**                      | Body fetch dominates total time; avoids $4\times$ wire and extraction penalty on sketches.                                                                                                                         |
| **ID-Only Sync (No Locator-Collision Fallback)** | **Option A ($\mathrm{GF}(2^{256})$)**             | No locator collisions; avoids collision-resolution fetches when decoding completes within budget for $d \le C$. Capacity overflow, budget exhaustion, and the reserved all-zero digest can still require fallback. |
| **ID-Only Sync (Practical)**                     | **Option B2 ($128/128$, $\mathrm{GF}(2^{128})$)** | Reuses `h128` residual as $V_0$; $2^{64}$ adversarial floor; avoids degree-256 root finding.                                                                                                                       |
| **GF($2^{64}$)-Only Hardware Constraint**        | **Option B1 ($64/192$, $\mathrm{GF}(2^{64})$)**   | Preserves existing SIMD paths; must document $2^{32}$ adversarial collision fallback.                                                                                                                              |

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

Insertion returns `ZeroSketchElement` for the all-zero full digest; the caller
must route it through the non-algebraic fallback for exact coverage (no `0 -> 1`
aliasing on the full element). Drop the `h64 == 0` guard from
`ResidentKernel::insert`; bucket placement may use a zero prefix. Keep the
zero-element rejection in `SyndromeSketch::toggle`, but apply it to the full
32-byte digest. The current first-non-zero scan makes `h64 | q3 | h128` disjoint
only probabilistically; fixed windows make it strict. `FixedWindowElementHash`
now represents this mapping explicitly, separately from the existing
`ElementHash`; the GF(2^256) path uses it, and the existing path keeps
`ElementHash`.

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
`x^256 + x^10 + x^5 + x^2 + 1`. Elements are four little-endian `u64` limbs, bit
`i` is the coefficient of `x^i`, and the byte mapping is little-endian. The
reduction polynomial is justified by Rabin's irreducibility test in the
`binary_field` test module, next to a reducible-polynomial negative control.

`MAX_FACTOR_WORK_GF256` is 300,000,000 work units. Release measurements for
degree-256 inputs were 232,591,912 units for consecutive roots and 36,127,278
for pseudorandom roots. This is a practical ceiling, not a proven worst case:
the work model's full binary recursion bound is 5,148,287,480 units (a chain of
`(1, d-1)` splits maximizes the model), and constructing an input that reaches
it is still open. When the budget is exhausted, `Gf256Decoder::decode` returns
`AlgebraicError::BudgetExhausted`; fallback selection (such as falling back to
the non-algebraic path or the fallback ladder) is the responsibility of the
caller / integration layer, as the decoder does not invoke a fallback
internally. Default selection and server-side sketch building are live, but the
bucket exchange driver is not yet used in production.

Before the reduce/square optimization, the 232.6M consecutive case took about
4.78 s and the 36.1M random case about 1.54 s. Those imply different observed
rates (about 49M and 23M work units/s), so one throughput number cannot predict
all decode times. After the optimization, the reported degree-256 times are
about 2.76 s consecutive and 0.90 s random; these are host-specific. Linear
scaling of those optimized runs puts a 300M-unit decode near 3.6–7.5 CPU seconds
and the 5.15B model bound near 61–128 seconds, depending on which workload rate
is used. These are rough risk estimates, not runtime guarantees: work units are
an accounting model, not a uniform measure of CPU time. The ceiling is a DoS
surface, so keep it per attempt and never calibrate it from random roots alone.

On `no_std` platforms or architectures without hardware carry-less
multiplication (PCLMULQDQ / AVX-512), multiplication falls back to software
bitwise algorithms (`clmul_raw_bitwise`), which are roughly 10–20× slower. While
hardware-accelerated decode handles up to 300M units in ~3.6–7.5 seconds,
unaccelerated software multiplication on `no_std` can cause long synchronous
decode times (tens of seconds to minutes) before reaching either success or
`BudgetExhausted`. Callers deploying on `no_std` or within synchronous event
loops must account for this latency by configuring tighter work allowances or
explicit timeouts.

The 6.4x consecutive-versus-random gap in work units at F = 256 is not
explained. No comparable gap has been observed in the tested F = 64 workloads,
but that comparison is not a general result over all root sets. Do not
extrapolate budgets from random roots until the gap is understood; the present
ceiling is anchored on the higher-work structured case.

The earlier analytical estimate for d = 256 was 51,380,224 units. The measured
consecutive case is 232.6M, about 4.5x higher, so the model underestimates the
number of splitting rounds, not only the per-multiply cost. Treat the analytical
ratio table above as a lower bound.

### 8.4 Cost, latency and memory

| Metric                          | Before                                                                                                                       | After                                                                             |
| :------------------------------ | :--------------------------------------------------------------------------------------------------------------------------- | :-------------------------------------------------------------------------------- |
| ID-only sync latency            | 2 RTT                                                                                                                        | 1 RTT (one fewer RTT; not necessarily a 50% wall-clock drop)                      |
| Event replication latency       | 2 RTT if ID and body are batched in phase 2; otherwise 3 RTT (sketch, IDs, bodies)                                           | 2 RTT (sketch, bodies); body fetch remains                                        |
| Post-decode verification        | O(d·C) re-encode, O(d) count/residual, local classification (O(d log N) or amortized O(d)), plus candidate/ambiguity lookups | Same, minus the candidate map and ambiguity branches. Not O(1)                    |
| Root finding work units         | 16,000,000 ceiling                                                                                                           | 300,000,000 practical ceiling; measured 232.6M consecutive, 36.1M random at d=256 |
| Full decode wall clock          | —                                                                                                                            | ~2.8 s consecutive, ~0.9 s random at d=256 (release host)                         |
| Wire per bucket sketch (C = 32) | 256 B                                                                                                                        | 1024 B                                                                            |
| Resident strata                 | 2048 B, F = 64                                                                                                               | unchanged                                                                         |

The GF(2^256) implementation uses nine carry-less base multiplies via Karatsuba,
followed by reduction. Its dedicated square spreads bits before reduction. In
the current benchmark run these took about 22 ns and 16 ns per operation;
earlier probes measured 45 ns and 39 ns before the reduce/square rewrite, and 29
ns and 18 ns after it. Timings vary with host and harness.

| Constant                 | Today      | Option A                                              |
| :----------------------- | :--------- | :---------------------------------------------------- |
| `MAX_FACTOR_WORK`        | 16,000,000 | 16,000,000 (GF(2^64) strata)                          |
| `MAX_FACTOR_WORK_GF256`  | n/a        | 300,000,000 practical; 5,148,287,480 model worst-case |
| `MAX_BATCH_FACTOR_WORK`  | 36,700,160 | ~344,981,504 provisional; not re-derived for F=256    |
| `MAX_STRATA_FACTOR_WORK` | 1,458,176  | 1,458,176 (F=64, d=8)                                 |

`MAX_BATCH_FACTOR_WORK` is still the old F = 256 extrapolation and needs a
measured re-derivation. The sketch-gated accumulator forgery bound has also not
been re-derived for the wide-field profile; the table in §8.2 records it as
open. Neither figure should be treated as a settled budget.

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
   `MAX_FACTOR_WORK` all depend on `FIELD_BITS`. Use F = 256 for the default
   full-ID bucket sketches and F = 64 for strata and the existing path; do not
   set one global `FIELD_BITS = 256`. **Decoder genericization, the GF(2^256)
   field profile and the default sketch, extraction and verification path are
   implemented.** Transport wiring remains pending.
2. Keep prefix bisection, capacity overflow, budgets, residual/count checks and
   the prefix-grinding policy. The GF(2^256) path does not need
   `SortedPopulation::candidates`, ambiguity branches or phase-2
   `verify_follow_up`, because it recovers full digests; it verifies locally.
   Those pieces stay in place for the existing GF(2^64) path.
3. Strata key stays `digest[8..16]`; never derive it from `h128`. Fixed-window
   digest representation is implemented and used by the default GF(2^256) path.
4. **Benchmarks:** a runnable suite entry covers GF(2^256) multiply, square,
   full-ID set sketch construction, cached decode, and room exchanges at several
   difference sizes. Full-ID extraction/exchange is pending production wiring.
   `benches/bench-results.csv` remains GF(2^64)-only; GF(2^256) work-budget
   calibration is not recorded there and remains open (see `docs/tech_debt.md`).
5. DX sketch: O(N) fingerprint filter; does not recover 256-bit IDs.
   Experimental comparison in `filter_spillover` only, not `triage`.

### 8.6 `algebraic_gf256`

`algebraic_gf256` is the bucket-sketch algorithm for the full-digest, ID-only
reconciliation path and is selected by default for new callers through
`select_default_action`. It coexists with the existing GF(2^64) bucket sketch,
which callers can still select explicitly. There is no automatic fallback
between the two. This format has not been released, so its name has no version
suffix. Use `algebraic_gf256` as the default profile identifier and as a wire
discriminator if the transport needs one.

The field polynomial, element mapping, byte order, and syndrome encoding below
define this format. A future incompatible format may receive another name if and
when one exists.

**1. Element mapping.** For room-version 3 and later, use the raw 32-byte
base64-decoded event-ID digest: `Gf256::from_le_bytes(digest[0..32])`, with no
transformation. The digest is split into four consecutive 8-byte chunks, each
interpreted as a little-endian limb; the least-significant limb is therefore
`u64::from_le_bytes(digest[0..8])`. This is a bijection from 32-byte digests to
field elements, so a decoded element maps back to the original digest bytes and
event ID. The profile removes truncation-induced locator collisions for distinct
digests; identity still inherits SHA-256's collision bound. PinSketch's set
universe is the nonzero field elements
([construction over GF(2^m)\*](https://courses.csail.mit.edu/6.857/2009/handouts/DodisReyzinSmith-FuzzyExtractors.pdf)),
so the all-zero digest is a valid field element but an invalid set element;
reject it at insertion and element toggle. Do not alias it to one. If this
otherwise valid digest occurs, abandon algebraic reconciliation for the room and
use the caller's direct event-ID difference fallback. Zero syndromes remain
valid.

**2. Zero placement prefix.** The placement key is
`u64::from_be_bytes(digest[0..8])`, matching the existing big-endian prefix
ordering. Zero is legal as a placement prefix. This is independent of the full
field element's nonzero requirement and byte interpretation.

**3. Strata key.** Keep strata in GF(2^64), keyed by the big-endian value
`u64::from_be_bytes(digest[8..16])` and the existing `0 -> 1` estimator alias.
It affects only estimation; the tree remains the recovery path. `q3` can be
derived from the stored full digest.

**4. Residual.** `h128` is `u128::from_be_bytes(digest[16..32])`, and
`NodeSummary` remains its existing 16-byte digest plus count. These big-endian
window interpretations are independent of the little-endian four-limb field
interpretation of the complete digest.

**5. Wire framing.** Carry the algorithm identifier and capacity in the
authenticated request body when the transport needs to distinguish this format
from existing GF(2^64) sketches. Require the payload length to be exactly
`32 * C`. This framing identifies one format; it does not negotiate
alternatives. Matrix federation authenticates requests with X-Matrix signatures
and responses with the TLS server certificate
([Server-Server API authentication](https://spec.matrix.org/latest/server-server-api/#authentication)).

**6. Payload size.** The binary bucket payload is `32 * C` bytes: 1024 B at
`C = 32` and 8192 B at `C = 256`. Base64url expands these to about 1.4 KB and 11
KB, respectively, before the header and surrounding protocol envelope. Use
`C = 32` for normal buckets and `C = 256` only for overflow under explicit local
resource policy. Prefer prefix splitting when possible, enforce the per-attempt
work ceiling, and rate-limit costly overflow attempts. Do not raise
`MAX_BATCH_FACTOR_WORK` from its old extrapolation without measured GF(2^256)
re-derivation.

**Performance and alternatives.** `algebraic_gf256` is not the fastest profile;
it is the full-digest profile. Current measurements are about 22–29 ns per
multiply, 16–18 ns per square, and 0.9–2.8 s for degree-256 decode, with
substantial host and workload variation. PinSketch decoding is quadratic in the
difference size, so keep ordinary buckets small and treat overflow as a bounded
expensive path. A GF(2^128) correlated-payload design may be cheaper, but is not
yet implemented or benchmarked; do not claim a performance win without measuring
it. IBLT/Erlay schemes are separate short-ID or probabilistic trade-offs, not
equivalent full-ID profiles. Erlay uses per-link salted short transaction IDs
and bisection/fallback handling; it does not recover full IDs from its initial
short-ID sketch ([Erlay paper](https://nt4tn.net/papers/ccs19-erlay.pdf)).

**Implementation status.** `FixedWindowPopulation` and `Gf256Sketch` can build
bucket sketches and extract a full-digest symmetric difference. The existing
MSC0501 client state machine and network adapter use the GF(2^64) bucket path
and are unchanged; the GF(2^256) path sits beside them, and wiring its payload
into the transport remains pending. No profile negotiation is needed while only
`algebraic_gf256` is supported. Keep a small format header only if the transport
needs to reject existing-path payloads explicitly; place it with wire framing,
not in profile negotiation machinery.
