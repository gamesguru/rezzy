> From: https://claude.ai/chat/a542bc08-f905-431e-a7c7-a185fcceb31f

# you asked

[text/rust] hash.rs

[text/rust] audit.rs

[text/rust] codec.rs

[text/rust] gc.rs

[text/rust] delta.rs

[text/rust] mod.rs

[text/rust] tests.rs

is the compressed HAMT the optimal state store?

analyze the precise read/write complexities for point updates/reads, delta isolation(s), and bulk rewrites.

how does it compare to conduwuit's `shortstatehash` or Synapse's `state_groups`?

---

# claude response

## Short answer

No, not in an absolute sense. It sits at one specific point on the trade-off curve, and for state resolution that point is a good one: point operations cost O(log₃₂ S), the conflicted set costs work proportional to how far the states have diverged rather than to S, and identical states produce identical roots. You pay for that in three places:

1. **Write amplification.** Every published root copies a full spine.
2. **No key order.** There are no type-scoped range reads.
3. **GC.** Garbage collection effectively requires a full mark-sweep, because Matrix root histories branch.

I also found a correctness bug in `diff_nodes` that affects the most common adjacent-root delta. It's described in the last section, and it matters before any of the complexity analysis does.

## Cost model

Notation:

- **S**: number of state entries.
- **d**: expected depth, ≈ log₃₂ S + 1. That is about 4.3 for S = 10⁵, with a hard cap of 52.
- **B**: encoded node size.
- **e**: encoded entry size. For a key of `(EventType, state_key)` plus an event-id value, e is roughly 60–100 B.
- **k**: number of differing keys between two states.

**Node count.** A 32-ary trie with n keys has about n / ln 32 ≈ 0.29·S internal nodes (Knuth's M-ary trie result; CHAMP inlines singletons, so the counts match).

**Spine size at S = 10⁵.**

| Level | Shape | Approx. bytes |
|---|---|---|
| L0, L1 | Saturated: 32 child hashes | ≈ 1 KB each |
| L2 | About 30 occupied slots, about 5 inline leaves | ≈ 1 KB |
| L3 | About 3 leaves | ≈ 150–300 B |

**One published root therefore costs about 3–4 KB of new nodes.**

| Operation | Resident | Cold (lazy resolver) |
|---|---|---|
| Point read | 1 BLAKE3 of the key, plus d popcount steps | d sequential fetches, each with an O(B) verify hash. `descend_level` turns this into d batched round trips for many keys. |
| Point write (insert/remove) | d node rebuilds. Each rebuild clones leaves and children, encodes, and hashes: Θ(d·B) | Up to d resolves on the path |
| Bytes written per published root | Θ(d·B), about 3–4 KB | n/a |
| Adjacent delta (k keys) | Visits Σ_ℓ min(k, 32^ℓ) node pairs per side, roughly O(k·d), with O(32) leaf comparisons each | Up to 2 fetches per visited pair |
| Arbitrary-pair delta | O(N_a + N_b) = O(S) when the states share no structure | Same, cold |
| n-way conflicted set | Union of deltas against a base state s₀ gives exactly the conflicted keys, at O(Σ k_i · d) | Same |
| Full-state read | O(S + 0.29·S) nodes | Same |
| Type-filtered read (e.g. all members) | **O(S).** Routing destroys key order, so there is no range scan. | Same |
| Bulk build (`build_hamt`) | S key hashes + O(S·d) bucket moves + hashing all node bytes ≈ O(S·e) | n/a |
| `persist_mutations` with m changes | Θ(m·d·B), because it does m sequential path copies, hashes intermediate nodes and throws them away, then runs a diff and a DFS | n/a |
| Space per full snapshot | S·e plus about 20% node overhead | n/a |

On the crossover between incremental and rebuild at S = 10⁵: a sequential batch costs about m × 3 KB of node bytes, while a full rebuild costs about 9 MB. The crossover is therefore around **m ≈ 3% of S**.

A sorted, prefix-grouped batch apply would touch only Σ_ℓ min(m, 32^ℓ) nodes, with each touched node rebuilt once. That beats both strategies across nearly the whole middle range of m. For m = 1000, it is roughly 1.6k node rebuilds versus 4k for sequential path copies.

## Comparison

A caveat on the conduwuit column: it is based on the `state_compressor` I know. That code keeps at most about 3–4 diff layers and merges a new layer into its parent when `diffsum² ≥ 2·diff_to_sibling·parent_diff`. Continuwuity may have diverged since, and you'll know that code better than I do.

| | rezzy HAMT | Synapse `state_groups` | conduwuit `shortstatehash` |
|---|---|---|---|
| Identity / dedup | Canonical root, so equal states give equal roots | None. Equal states get different group ids. | Hash of the sorted compressed full state, so it dedups |
| Bytes per transition | Θ(d·B), about 3–4 KB | \|Δ\| rows, plus a full snapshot whenever the chain would exceed 100 hops (`MAX_STATE_DELTA_HOPS`). **Amortized Θ(\|Δ\| + S/100) rows**, which at S = 10⁵ is about 1000 rows, or 100 KB+ per transition. | About 16 B × \|Δ\|, plus periodic layer merges that can rewrite Θ(S) |
| CPU per write | Θ(d·B) | O(\|Δ\|) plus a hop count | **Θ(S)**: it builds the full compressed set, hashes it, and diffs it against the parent |
| Point read (cold) | d fetches | Recursive CTE over up to 100 edges, then an index probe per group in the chain: O(h·log N) with h ≤ 100 | **Θ(S)**: it reconstructs `full_state` from up to about 4 layers and then looks the key up |
| Point read (warm) | d pointer hops | Dictionary cache hit | O(log S) or O(S), depending on how `CompressedState` is stored |
| Full read | O(S) | O(S + Σ deltas over the chain), with `DISTINCT ON` | O(S) |
| Type-filtered read | **O(S)** | O(h·log N + r) via the `(type, state_key)` index | O(S) |
| Delta, parent to child | O(k·d) | O(\|Δ\|), read directly from rows when stored as a delta | O(\|Δ\|) if the layers line up |
| Delta, arbitrary pair or n-way | **O(Σ k_i · d)** | Θ(n·S): load every full map and compare | Θ(n·S) |
| Memory with many cached states | Shared subtrees | Each cached map is separate | Each layer's `full_state` is its own set, so roughly S × 16 B per cached state |
| GC | Content-addressed sharing. Refcounting is only sound for linear histories (see your own `gc.rs` doc), so in practice it needs a full mark-sweep. | Cascading purges, plus the external compressor | Effectively never |

On writes, under the stated assumptions the HAMT writes fewer bytes per transition than Synapse's amortized snapshot-plus-delta scheme on large rooms. That is a bytes-written comparison only: it does not account for CPU cost, and it loses on bytes per transition to an idealized pure-delta scheme. It dominates both on point reads and divergence-proportional state resolution, excluding type-filtered reads.

The one place it is clearly worse than Synapse is **type-filtered reads**. That covers lazy-loading members, `/members`, and `StateFilter`-style queries in general.

## Where it isn't optimal, and what to do about it

**1. Space per update is not optimal, and the cost is buying canonicity.**

Driscoll–Sarnak–Sleator–Tarjan node-copying achieves O(1) amortized space per update with O(1) access slowdown, including for full persistence. Path copying costs Θ(d·B).

What you get for the difference is three things:

- O(1) root equality,
- content-addressed dedup,
- verifiable nodes.

That trade is right for state resolution, but you are currently paying it for every intermediate state group, most of which are never read.

**Fix:** represent a state group as `(materialized base root, delta list of length ≤ L)` and materialize a root every L transitions or on first read. Your identity layer already supports this. `state_group_id` comes from the LtHash, which is homomorphic and maintainable without a root; it does not come from `structural_hash`. Dedup survives, intermediate spines shrink by roughly L×, reads cost O(d + L), and the conflicted-set computation is unchanged up to O(L).

The catch is that this conflicts with `persist_chain`'s "every step publishes a root" contract, so that contract would need to change.

**2. There is no key order. This is the real functional gap.**

You don't need prolly trees or MSTs to fix it. Your `key_hash` hook is enough: route with

```
path = H(type)[..10 bits] ‖ H(type, state_key)[10..]
```

That places each event type under a stable 2-level prefix. A filtered read for one type then costs O(d + r), at the price of about one extra level for `m.room.member` and lumpy fanout near the root.

Prolly trees or MSTs keyed on `(type, state_key)` would give you true ordered ranges with the same canonicity and diff properties. They come with chunk-boundary churn on writes and greater depth variance. Fanout is a non-issue either way: a B-tree's log₁₀₀ versus your log₃₂ is 2.5 versus 3.3 levels at S = 10⁵.

**3. Batched mutation.** Apply the batch grouped by path prefix so that each touched node is rebuilt once. Fall back to `build_hamt` when m is above about 3% of S.

**4. Write-path constant factors.** On every created node, the current code does all of the following:

- `compute_structural_hash` allocates `child_hashes`, encodes the body, and copies it into `buf`.
- The persist sink then runs `PersistedInternalNode::from`, which **clones every leaf**, including the `String` keys.
- It then encodes the node a second time.

Return the canonical bytes from `rebuild_node` and reuse them. That is one encode, no leaf clone, and roughly half the per-node cost.

On the read side, `into_hamt_node_verified` re-encodes the node just to hash it. If the codecs are canonical (add a test that `encode(decode(b)) == b`, including `EventType::from`), you can hash the fetched buffer directly.

**5. Internal pointer width.** 32-byte child references make up about 90% of the bytes in the upper levels. Local 8-byte node ids with a separate hash-to-id dedup index would cut spine bytes roughly 3×. The cost is that per-node verify-on-read then needs the hash stored alongside the node. Whether that's worth it depends on whether you actually trust local storage.

## Issues in the code that change the answer

> **Status (historical, fixed):** the `diff_nodes` leaf-versus-subtree bug described below was fixed in `src/hamt/delta.rs` (commits `dd3c7b09`, `4def5acf`) and is covered by `test_isolate_delta_leaf_subtree_transition`. The diagnosis is kept as originally written.

**Real bug: `diff_nodes` mishandles a slot that holds a leaf on one side and a subtree on the other.**

The datamap loop and the nodemap loop are independent. When A has a leaf `(k, v)` at slot s and B has a child at s, the datamap loop emits `removed (k, v)`. The nodemap loop then walks B's subtree and emits `added (k, v)` along with the genuinely new key.

This is exactly the result of **every insert that splits a leaf**, so any delta between adjacent roots after an ordinary collision insert is wrong. Here is what each kind of consumer sees:

| Consumer | Effect |
|---|---|
| LtHash arithmetic | Cancels out, so no visible error |
| Applies removes, then adds | Correct by accident |
| Treats the output as a set of changed keys (sync, notifications, conflicted set) | Reports a spurious change |
| Applies adds, then removes, by key | **Deletes live state** |

Your randomized oracle test misses this for two reasons. Values are drawn from 0..1000, so a shared key with an identical value occurs only about once per 1000 shared keys. And nothing runs `isolate_delta` after an `insert` that splits a leaf.

Separately, the "skip on collision" branch in that test is noise. `build_hamt` only fails on a full 256-bit path collision, so the `completed >= 20` floor is guarding against something that can't happen. Replace the skip with `expect`.

Fix: handle the cross-kind slots explicitly and remove them from both existing loops.

```rust
let leaf_a_sub_b = d_a & n_b;
let sub_a_leaf_b = n_a & d_b;
let cross = leaf_a_sub_b | sub_a_leaf_b;
// iterate set_bits((d_a | d_b) & !cross) and union_nodemaps(n_a & !cross, n_b & !cross) as before, then:
for slot in set_bits(leaf_a_sub_b) {
    let (k, v) = &node_a.leaves[map_index(d_a, slot)];
    let sub = resolve_only_child(node_b, n_b, slot, resolver)?;
    let mut same = false;
    walk_subtree(&sub, resolver, next_depth, &mut |n| {
        for (kb, vb) in &n.leaves {
            if kb == k && vb == v { same = true; } else { added.push((kb.clone(), vb.clone())); }
        }
    })?;
    if !same { removed.push((k.clone(), v.clone())); }
}
// mirror for sub_a_leaf_b, swapping the roles of added/removed
```

For the regression test, build `a = {k1: 1}` and `b = insert(a, k2)`, where k2 shares k1's root slot. Assert that the delta is `([(k2, 2)], [])`. Then shrink the property test's value range to something like 0..4 so the same-value case comes up constantly.

**Medium: lookups conflate corruption with absence.**

- `search_by_path_hash` returns `Ok(None)` after running past `HAMT_MAX_DEPTH`, and `test_hamt_search_by_path_hash_rejects_excessive_depth` pins that behaviour.
- `get` is the fully materialized API; `search` is the lazy API and returns
  resolver errors when a child cannot be loaded.

Auth callers that need lazy reads should use `search` so "no power_levels
event" and a resolver failure remain distinct.

**Doc accuracy in `hash.rs`.** The claim that grinding k ≤ 10 levels is "practical (seconds)" overstates the attack. 2^(5·10) = 2^50 hashes is GPU-days, not seconds; "seconds" holds for roughly k ≈ 5–6. Matching any of the S existing keys divides the cost by S. The conclusion still stands, since depth is bounded at 52 and the damage is a slowdown, but you'll want the numbers right if this threat model ends up in a paper.

---

# you asked

so does this improve the typed read performance to a competitive or superior level? If not, could an alternative data structure entirely?

Can you point do, research, or find out an alternative to the HAMT/CHAMP which is strictly superior in typed reads and at least equal or better in all other areas? Draw on your vast knowledge and researches.

---

# claude response

## Does the type-prefix fix make typed reads competitive?

It helps for small event types. It does not help for the one typed read that is expensive, which is scanning all members, and the prefix version brings a vulnerability of its own.

Assume S = 10⁵ entries, of which r_member ≈ 10⁵ are membership events, a typical room with about 30 distinct event types, entries of about 80 B, and cold storage throughout.

| Typed read | HAMT as it is today | HAMT with type-prefix routing | Synapse (`state_groups`) | conduwuit |
|---|---|---|---|---|
| A small type (power levels, join rules, and so on) | Full scan, Θ(S) | About 2–3 node fetches | Up to 100 index probes along the delta chain | Θ(S) to reconstruct, then a lookup |
| One member by state_key | d ≈ 4–5 fetches | d + 1 or 2 fetches. The extra levels are single-child chains, because CHAMP has no path compression. | Up to 100 probes | Θ(S) |
| All members | Θ(S) | **About 0.29·r ≈ 29k random node fetches** | About 2k mostly sequential pages, because a snapshot's rows sit together in the heap, plus the delta chain | Θ(S) |
| Everything except members (the lazy-loading case) | Θ(S) | O(size of the non-member state). The member subtree is skipped entirely. | Index range scans along the chain | Θ(S) |

Small-type reads become better than both alternatives. The full member scan stays about 15× behind Synapse in fetch count.

The reason is structural. HAMT nodes near the bottom of the tree are tiny, averaging about 3.4 entries each, so any walk over a subtree costs roughly one fetch per 3–4 entries. An ordered tree with ~4 KB leaves gets about 50 entries per fetch. Routing by type changes which subtree you walk, not how expensive the walk is.

The prefix approach also has a real problem: **a 10-bit type prefix can be ground in about 2¹⁰ attempts.** Anyone allowed to send custom state events can choose a type whose prefix matches `m.room.member`, then fill it with entries. Every member scan then has to walk the attacker's entries as well. Lengthening the prefix to make grinding expensive adds a single-child node per extra 5 bits to every path, because there is no path compression.

The better version of the same idea is a **nested exact map**: an outer map from type to the root of an inner HAMT keyed by state_key. It cannot be collided, a small-type read costs one outer lookup plus a tiny inner tree, and skipping members is exact. The cost is that every write also rewrites the outer node. If types are interned to u32 ids locally, that node is about 1.1 KB, which adds roughly 25% to the bytes written for a member change. The member scan constant does not change.

## Is there a structure that is strictly better everywhere?

No. Under the scheme you use now, where every published state group gets its own path-copied root, none can exist. There are three reasons, and all three are inherent rather than engineering gaps.

1. **Leaf size pulls scans and writes in opposite directions.** A scan costs roughly r divided by the number of entries per leaf. A path copy writes at least one full leaf plus every internal node above it. To beat the member scan you need fat leaves of around 50 entries. To keep the 3–4 KB spine you need the bottom nodes HAMT has now, around 3 entries. No leaf size gives both.

2. **Ordering costs separator keys.** A HAMT internal node stores only a bitmap and child hashes, because routing is implicit in the key's hash. An ordered tree's internal nodes must also store separator keys. Even with prefix truncation down to 10–20 B per separator, each internal level ends up about 1.5–2× larger.

3. **Ordered and canonical together means randomized balance.** A canonical shape that depends only on the set of keys (the "uniquely represented" or history-independent property) requires randomized balancing for ordered external-memory trees. The B-treap and B-skip-list (Golovin, 2009–2010) and the Merkle Search Tree (Auvolat & Taïani, SRDS 2019) all get their depth and node-size bounds only with high probability. HAMT has hard bounds: at most 32 slots per node and at most 52 levels, for any input.

   With a public hash, the probabilistic structures can be attacked. Keys can be chosen to land at level 0 in an MST, or to avoid creating a chunk boundary in a prolly tree, and a run of such keys that sort next to each other forms one giant node. Rewriting that node on every insert is an O(r) write-amplification attack. HAMT's worst case is a bounded slowdown.

There is also a broader result: the Brodal–Fagerberg lower bound (SODA 2003) shows that for external-memory dictionaries, the trade-off between insert cost and query cost is tight. So the realistic question is not whether some structure dominates HAMT, but whether HAMT sits on the best achievable curve for Matrix's workload.

It doesn't. What lets you move off the curve is changing *how often you materialize a tree*, not changing which tree you use.

## Candidates

All of the ordered options below assume one change that applies to your existing design too.

**Your structural hash is local-only.** The `hash.rs` doc says it is not a wire format. That means the routing or level hash can be keyed with a **persisted secret that belongs to this server** instead of `room_id`. Doing so removes grinding as a threat for every content-defined structure, including the current HAMT, because an attacker can no longer compute where a key will land. This is the single most important enabler in this section. It requires a one-time migration, and the secret must be kept forever.

| | Typed scan | Point read (cold) | Bytes written per version | Diff | Canonical | Worst-case bounds |
|---|---|---|---|---|---|---|
| CHAMP as it is | Θ(S) | log₃₂ S | **≈ 3.5 KB** | O(k·d) | Yes | **Hard** |
| CHAMP with type prefix | Small types fast; members 0.29·r | +1–2 levels | ≈ 3.6 KB | O(k·d) | Yes | Hard, but the prefix can be ground |
| Nested type map over CHAMP | Small types exact; members 0.29·r | +1 level | ≈ 4.3 KB | O(k·d) | Yes | Hard |
| MST or B-skip-list on (type, state_key), secret level hash | **≈ r/32** | log₃₂ S | ≈ 6–9 KB | O(k·log S), output in key order | Yes, given the secret | With high probability |
| Prolly tree, secret boundary hash, ~4 KB chunks | **≈ r/50, sequential** | ≈ log₆₄ S (2–3 levels) | ≈ 8–12 KB | O(k·log S) | Yes, given the secret | With high probability; a maximum-chunk rule bounds node size but causes boundary cascades |
| Merkle Patricia trie on raw keys | Natural prefix scan | Depth depends on the keys | Small | O(k·d) | Yes | **None.** Long shared state_key prefixes produce O(r) depth. Reject it. |
| Any of the above, stored as a checkpoint plus a delta of ≤ L changes | Same as the underlying tree, plus O(L) | +1 fetch for the delta | **O(\|Δ\|), ~100 B**, plus the checkpoint amortized over L | O(k·log S + L) | Identity still holds, via LtHash | Same as the underlying tree |

The last row is the only design that improves both typed reads and bytes written per transition. It does so by combining fat ordered leaves, which favour scans, with delta versioning, which keeps writes small. This is the Bε-tree and LSM idea applied to versioning.

It is still not strictly better on every axis. Point reads pay O(L) plus one extra fetch, and each checkpoint writes more bytes than a CHAMP path copy would. With L = 8–16, both regressions are small and bounded. Your identity layer already supports it, because `state_group_id` comes from the homomorphic LtHash, not from the root hash.

## Recommendation

**1. Get full-member scans off the versioned store.** Expensive scans over all members almost always apply to *current* state: `/members` without an `at` parameter, room summaries, member counts. Synapse answers those from `current_state_events` and its membership tables, not from `state_groups`, and conduwuit has its joined-member tables. A mutable ordered index of current state, plus counts stored as subtree aggregates (Merkle trees can carry monoid annotations), removes the main case where HAMT loses. Typed reads against historical state are mostly lazy-loading reads, which are point lookups plus "everything except members", and a nested type map makes those fast.

**2. Versioned store: nested exact type map over CHAMP, with a secret routing key, stored as checkpoint plus delta.**

- You keep the hard worst-case bounds, which matter when the input is attacker-controlled state.
- Small-type reads and the "everything except members" read are exact and fast.
- Bytes written per transition are lower than today.
- When you materialize a checkpoint, lay its nodes out in DFS order. That makes the 0.29·r member walk largely sequential, because cold-scan cost depends on storage layout as much as on tree shape.

**3. Switch to an ordered MST or prolly tree only if you have measured that historical full-type scans are a real bottleneck.** It is the right structure for that workload. You would trade hard bounds for bounds that hold with high probability given a secret salt, and you would write roughly twice as many bytes per checkpoint.

The headline answer: no single structure strictly dominates CHAMP. Changing the structure only moves you along the scan-versus-write curve. What actually improves on it is the combination of separating current-state scans from history, using delta versioning, and keying the hash with a secret, and that combination is better than any swap of tree type.
