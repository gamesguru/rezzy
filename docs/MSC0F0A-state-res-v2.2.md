# MSC0F0A: State Resolution v2.2 (Fixing Pathologies in v2/v2.1)

## Background

Matrix State Resolution v2 (and v2.1) introduced robust algorithms for resolving divergent state across federated servers. However, recent large-scale room collapses have revealed critical structural vulnerabilities and pathologies in the algorithm's handling of missing or conflicting auth events.

Specifically, two real-world incidents motivated this spec change:

- **Anomaly 1 (`!DGMOlu` / Presence v2):** A matrix.org server dropped users `@gg:nutra.tk` and `@shane:wombatx.me` because a massive fork merge lacked the `m.room.create` auth event in its context. This caused the state resolution algorithm to incorrectly strip the creator of implicit power, rejecting the merge entirely.
- **Anomaly 2 (`!tgmfqA` / Catgirl):** A server (`catgirl.cloud`) permanently lost the membership of `@bot:nutra.tk`. A state recalculation during a massive merge (or extremity reset) caused the bot's membership to land on a losing branch and be silently excluded from resolved state.

From these messy, real-world failures, we have abstracted three specific, generalized pathologies that State Resolution v2.2 mathematically resolves:

1. **The "Vanish" Anomaly (Connectivity Break)**
2. **The "Invite-Lock" (Join Rule Regression)**
3. **Duplicate Auth Poisoning**

## 1. The "Vanish" Anomaly (Connectivity Breaks & Missing Auth)

**Problem:**
When a server misses a critical auth PDU (e.g., a join event) due to federation hiccups, it experiences a "hole in the set". If a subsequent event (like a message) merges into the DAG and lists the missing auth event in its auth chain, State Res v2.1 performs a strict 1-hop check. If an intermediate auth event (like Power Levels) is omitted from the direct `auth_events` list but is present deeper in the chain, the event is incorrectly rejected (403 Forbidden).

**Solution (Recursive BFS Walk):**
State Resolution v2.2 modifies auth lookup. Instead of relying solely on the explicit 1-hop `auth_events` list of an event, the algorithm MUST perform a recursive Breadth-First Search (BFS) walk up the auth chain to discover required ancestor events (e.g., Power Levels) that might have been omitted directly but are implicitly authenticated by other events in the chain.

## 2. The "Invite-Lock" (Join Rule Regression)

**Problem:**
During a massive state storm or merge involving dormant, stale branches, State Res v2.0+ supplements the auth chain with resolved state. Because of the sheer size of the merge and missing PDUs, the algorithm can incorrectly favor state from a stale branch (e.g., picking an old `invite` join_rule over the current main timeline's `public` join_rule). This functionally locks the room.

**Solution:**
[TBD: We need to define the exact tie-breaking or topological sorting adjustments required in v2.2 to prevent stale branches from dominating the resolved state during massive merges.]

## 3. Duplicate Auth Poisoning

**Problem:**
When a server receives two conflicting events with the same state key (e.g., two concurrent joins for the same user on different forks) and inadvertently embeds BOTH into the `auth_events` of a new message, it poisons the auth chain. Strict servers hard-reject these events, causing permanent federation splits.

**Solution:**
State Resolution v2.2 strictly forbids duplicate state keys in the `auth_events` of an event. Protocol-level validation must immediately reject any event that attempts to list multiple auth events with the same `(type, state_key)` tuple, before state resolution even begins.

## Proposed Room Version

These changes are significant enough to warrant a new room version (e.g., Room Version 13) which defaults to State Resolution v2.2.
