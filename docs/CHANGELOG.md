# Changelog

## Unreleased

### Fixed

- **State resolution v2 (room versions 2-11): power-event classification.** Only
  `m.room.power_levels`, `m.room.join_rules`, and `m.room.member` kicks and bans
  (leave/ban where sender != state_key) are power events, as in the spec. Rezzy
  also classifies `m.room.create` as a power event; that is an implementation
  convenience, not a spec claim, and is harmless because the create key cannot
  be conflicted in a well-formed room. Previously every `m.room.member` was a
  power event in v2, so ordinary self-membership changes skipped the mainline
  ordering. V1 keeps routing all memberships early (its own membership pass).
- **Auth difference in v2.** `resolve_state_maps` and
  `resolve_state_maps_lazy_with_diff` now put the auth difference (`∪Cᵢ − ∩Cᵢ`)
  into the set they resolve.
- **Auth difference excluded from v1.** State resolution v1 (room version 1)
  bypasses auth difference calculation across concrete (`resolve_state_maps`),
  lazy (`resolve_state_maps_lazy_with_diff`), and `compute_state_at` paths,
  preventing auth-chain context events from being introduced into conflicted
  sets.
- **Auth-difference power events take part in authorization (v2).** An
  authorized auth-difference event now updates the partially resolved state
  during resolution, as in the spec and Synapse, so e.g. a power-levels event
  only one fork cites can authorize a later conflicted event. The result still
  restores the unconflicted state.

### Changed (breaking)

- `LeanEvent::is_power_event` and `route_power_events` take a `StateResVersion`.
- **Resolved state can differ** in rooms v2-v11 wherever ordinary memberships
  conflict (ties on mainline position and timestamp now break by event ID).
  Consumers that cache resolved state, such as Sithnapse's `_conflicted` cache,
  should clear it after upgrading.
