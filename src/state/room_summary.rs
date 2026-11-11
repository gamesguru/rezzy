//! Derived room-summary projections over one state group.
//!
//! These are pure functions over a caller-supplied [`SummarySource`]. They never
//! write, and the records they return are derived data: rebuildable,
//! evictable, and never part of `state_group_id` or the persisted HAMT.
//! Storage, caching, eviction, stream ordering, heroes, and viewer filtering
//! belong to the integrating homeserver. See
//! `docs/2026-11-11-room-summary-derivation.md`.
//!
//! The projections are split by cost:
//!
//! - [`derive_singletons`]: room-level auth event ids plus resolved display
//!   fields. About ten point reads and at most five event reads; it never
//!   touches a member event.
//! - [`derive_membership`]: joined and invited counts. One member walk.
//! - [`update_membership`]: counts for a child state group from its parent's
//!   counts and a complete state delta, validated against the child's
//!   `state_group_id` before any count changes.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use crate::basespec::event_types::EventType;
use crate::hamt::{state_group_id_from_lthash, StateGroupId};
use crate::incremental::LtHash;
use crate::{EventContent, EventId, LeanEvent};

/// Format version stamped into every derived record. Bump it when a field's
/// meaning changes so cached records from an older derivation are rebuilt.
pub const ROOM_SUMMARY_VERSION: u16 = 1;

/// Read access to one state group and the event store.
///
/// `get` and `for_each_member` read the state group being summarized.
/// `event` resolves any event id; `lattice` returns the retained `LtHash`
/// lattice of any state group (for `MTHR` roots, [`hamt_root::StateHamtRoot::lattice`](crate::state::hamt_root::StateHamtRoot::lattice)).
pub trait SummarySource<Id, C> {
    /// The caller's storage error.
    type Error;

    /// Event id at `(event_type, state_key)`, if present.
    ///
    /// # Errors
    /// Storage failures.
    fn get(&mut self, event_type: &EventType, state_key: &str) -> Result<Option<Id>, Self::Error>;

    /// Visits every `m.room.member` entry as `(state_key, event_id)`.
    ///
    /// # Errors
    /// Storage failures.
    fn for_each_member(&mut self, visit: &mut dyn FnMut(&str, &Id)) -> Result<(), Self::Error>;

    /// Resolves one event. A missing event is a storage error.
    ///
    /// # Errors
    /// Storage failures, including a missing event.
    fn event(&mut self, id: &Id) -> Result<Arc<LeanEvent<Id, C>>, Self::Error>;

    /// The retained lattice of `group`.
    ///
    /// # Errors
    /// Storage failures, including an unknown group.
    fn lattice(&mut self, group: &StateGroupId) -> Result<LtHash, Self::Error>;
}

/// Room-level auth event ids and resolved display fields.
///
/// Auth ids serve `auth_events` assembly; the display fields serve room lists
/// and headers. Sender membership is per-event and is not included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSingletons<Id> {
    /// The state group this record was derived from.
    pub state_group_id: StateGroupId,
    /// [`ROOM_SUMMARY_VERSION`] at derivation time.
    pub version: u16,
    /// `m.room.create` event id.
    pub create: Id,
    /// `m.room.power_levels` event id.
    pub power_levels: Option<Id>,
    /// `m.room.join_rules` event id.
    pub join_rules: Option<Id>,
    /// `m.room.name` `name`; empty is reported as `None`.
    pub name: Option<String>,
    /// `m.room.topic` `topic`; empty is reported as `None`.
    pub topic: Option<String>,
    /// `m.room.avatar` `url`; empty is reported as `None`.
    pub avatar_url: Option<String>,
    /// `m.room.canonical_alias` `alias`; empty is reported as `None`.
    pub canonical_alias: Option<String>,
    /// `m.room.encryption` is present with a string `algorithm`.
    pub encrypted: bool,
}

/// Joined and invited member counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MembershipCounts {
    /// The state group these counts describe.
    pub state_group_id: StateGroupId,
    /// [`ROOM_SUMMARY_VERSION`] at derivation time.
    pub version: u16,
    /// Members whose membership is `join`.
    pub joined: u64,
    /// Members whose membership is `invite`.
    pub invited: u64,
}

/// One side of a state change. There is no "unknown" side: a caller that
/// cannot resolve the old side must use [`derive_membership`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeSide<Id> {
    /// The key is known to be absent on this side.
    Absent,
    /// The key maps to this event id on this side.
    Present(Id),
}

/// One changed `(event_type, state_key)` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateChange<Id> {
    /// Event type of the entry.
    pub event_type: EventType,
    /// State key of the entry.
    pub state_key: String,
    /// The entry in the base state group.
    pub old: ChangeSide<Id>,
    /// The entry in the target state group.
    pub new: ChangeSide<Id>,
}

/// The complete state difference from `base` to `target`, across every event
/// type. A members-only delta does not reproduce `target` and is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDelta<Id> {
    /// The parent state group.
    pub base: StateGroupId,
    /// The child state group.
    pub target: StateGroupId,
    /// One entry per changed key.
    pub changes: Vec<StateChange<Id>>,
}

/// Why a summary could not be derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryError<E> {
    /// The state group has no `m.room.create` event.
    MissingCreate,
    /// The parent counts describe a different state group than `delta.base`.
    ParentMismatch {
        /// `parent.state_group_id`.
        parent: StateGroupId,
        /// `delta.base`.
        base: StateGroupId,
    },
    /// The parent counts were derived with a different summary version.
    ///
    /// The delta cannot update counts whose semantics changed, so rebuild with
    /// [`derive_membership`].
    ParentVersionMismatch {
        /// `parent.version`.
        found: u16,
    },
    /// A change whose two sides are equal (both absent, or the same event).
    EmptyChange {
        /// Event type of the change.
        event_type: EventType,
        /// State key of the change.
        state_key: String,
    },
    /// The same key appears more than once in a delta.
    DuplicateKey {
        /// Event type of the repeated key.
        event_type: EventType,
        /// State key of the repeated key.
        state_key: String,
    },
    /// Applying the delta to `base`'s lattice did not reproduce `target`.
    ///
    /// The digest cannot say why: a wrong base, a dropped change, or a false
    /// side all look the same. Rebuild with [`derive_membership`].
    DeltaMismatch {
        /// `delta.base`.
        base: StateGroupId,
        /// `delta.target`.
        target: StateGroupId,
        /// The digest the delta actually produced.
        computed: StateGroupId,
    },
    /// A validated delta would drive a count below zero, so the parent counts
    /// were wrong for `base`.
    CountUnderflow,
    /// The source failed.
    Source(E),
}

impl<E: fmt::Display> fmt::Display for SummaryError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCreate => f.write_str("state group has no m.room.create event"),
            Self::ParentMismatch { .. } => {
                f.write_str("parent counts do not describe the delta's base state group")
            }
            Self::ParentVersionMismatch { found } => write!(
                f,
                "parent counts use summary version {found}, expected {ROOM_SUMMARY_VERSION}"
            ),
            Self::EmptyChange {
                event_type,
                state_key,
            } => write!(
                f,
                "delta change for ({}, {state_key:?}) does not change anything",
                event_type.as_str()
            ),
            Self::DuplicateKey {
                event_type,
                state_key,
            } => write!(
                f,
                "delta lists ({}, {state_key:?}) more than once",
                event_type.as_str()
            ),
            Self::DeltaMismatch { .. } => {
                f.write_str("delta does not reproduce the target state group from its base")
            }
            Self::CountUnderflow => f.write_str("parent membership counts are inconsistent"),
            Self::Source(error) => write!(f, "summary source failed: {error}"),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for SummaryError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            _ => None,
        }
    }
}

/// Derives the room-level auth ids and display fields of the state group
/// `src` reads. Never resolves a member event.
///
/// # Errors
/// [`SummaryError::MissingCreate`] when the state has no create event, or
/// [`SummaryError::Source`] when a read fails.
pub fn derive_singletons<Id, C, S>(
    group: StateGroupId,
    src: &mut S,
) -> Result<RoomSingletons<Id>, SummaryError<S::Error>>
where
    Id: EventId,
    C: EventContent,
    S: SummarySource<Id, C>,
{
    let create = get(src, &EventType::RoomCreate)?.ok_or(SummaryError::MissingCreate)?;
    let power_levels = get(src, &EventType::RoomPowerLevels)?;
    let join_rules = get(src, &EventType::RoomJoinRules)?;
    let name = display_field(src, &EventType::RoomName, "name")?;
    let topic = display_field(src, &EventType::RoomTopic, "topic")?;
    let avatar_url = display_field(src, &EventType::RoomAvatar, "url")?;
    let canonical_alias = display_field(src, &EventType::RoomCanonicalAlias, "alias")?;
    let encrypted = match get(src, &EventType::RoomEncryption)? {
        Some(id) => event(src, &id)?
            .content
            .get_str_field("algorithm")
            .is_some(),
        None => false,
    };
    Ok(RoomSingletons {
        state_group_id: group,
        version: ROOM_SUMMARY_VERSION,
        create,
        power_levels,
        join_rules,
        name,
        topic,
        avatar_url,
        canonical_alias,
        encrypted,
    })
}

/// Counts joined and invited members of the state group `src` reads.
///
/// # Errors
/// [`SummaryError::Source`] when a read fails.
pub fn derive_membership<Id, C, S>(
    group: StateGroupId,
    src: &mut S,
) -> Result<MembershipCounts, SummaryError<S::Error>>
where
    Id: EventId,
    C: EventContent,
    S: SummarySource<Id, C>,
{
    let mut ids = Vec::new();
    src.for_each_member(&mut |_, id| ids.push(id.clone()))
        .map_err(SummaryError::Source)?;
    let (mut joined, mut invited) = (0_u64, 0_u64);
    for id in &ids {
        match tally(src, id)? {
            Tally::Join => joined = joined.saturating_add(1),
            Tally::Invite => invited = invited.saturating_add(1),
            Tally::Other => {}
        }
    }
    Ok(MembershipCounts {
        state_group_id: group,
        version: ROOM_SUMMARY_VERSION,
        joined,
        invited,
    })
}

/// Counts for `delta.target` from `parent` (the counts of `delta.base`).
///
/// The delta is validated before any event is read: it must be canonical (no
/// repeated key, no change whose sides are equal), and applying every change
/// to `base`'s lattice must reproduce `target`'s `state_group_id`. The count
/// update then applies exactly the same changes in the same direction, so a
/// validated delta yields the counts `derive_membership` would. The check
/// holds under `LtHash` collision resistance; it is validation, not a proof.
///
/// Costs one lattice copy, one lattice update per change, and up to two event
/// reads per changed member key.
///
/// # Errors
/// [`SummaryError::ParentMismatch`], [`SummaryError::ParentVersionMismatch`],
/// [`SummaryError::EmptyChange`], [`SummaryError::DuplicateKey`], or
/// [`SummaryError::DeltaMismatch`] for a delta that does not fit; the caller
/// rebuilds with [`derive_membership`].
/// [`SummaryError::CountUnderflow`] when `parent` was wrong for `base`, and
/// [`SummaryError::Source`] when a read fails.
pub fn update_membership<Id, C, S>(
    parent: &MembershipCounts,
    delta: &StateDelta<Id>,
    src: &mut S,
) -> Result<MembershipCounts, SummaryError<S::Error>>
where
    Id: EventId,
    C: EventContent,
    S: SummarySource<Id, C>,
{
    if parent.version != ROOM_SUMMARY_VERSION {
        return Err(SummaryError::ParentVersionMismatch {
            found: parent.version,
        });
    }
    if parent.state_group_id != delta.base {
        return Err(SummaryError::ParentMismatch {
            parent: parent.state_group_id,
            base: delta.base,
        });
    }

    let mut lattice = src.lattice(&delta.base).map_err(SummaryError::Source)?;
    let mut seen = BTreeSet::new();
    for change in &delta.changes {
        let ty = change.event_type.as_str();
        let key = change.state_key.as_str();
        if !seen.insert((ty, key)) {
            return Err(SummaryError::DuplicateKey {
                event_type: change.event_type.clone(),
                state_key: change.state_key.clone(),
            });
        }
        match (&change.old, &change.new) {
            (ChangeSide::Present(old), ChangeSide::Present(new)) if old != new => {
                lattice.replace(ty, key, old, new);
            }
            (ChangeSide::Absent, ChangeSide::Present(new)) => lattice.insert(ty, key, new),
            (ChangeSide::Present(old), ChangeSide::Absent) => lattice.remove(ty, key, old),
            _ => {
                return Err(SummaryError::EmptyChange {
                    event_type: change.event_type.clone(),
                    state_key: change.state_key.clone(),
                })
            }
        }
    }
    let computed = state_group_id_from_lthash(&lattice);
    if computed != delta.target {
        return Err(SummaryError::DeltaMismatch {
            base: delta.base,
            target: delta.target,
            computed,
        });
    }

    let (mut joined, mut invited) = (i128::from(parent.joined), i128::from(parent.invited));
    for change in delta
        .changes
        .iter()
        .filter(|c| c.event_type == EventType::RoomMember)
    {
        if let ChangeSide::Present(old) = &change.old {
            match tally(src, old)? {
                Tally::Join => joined = joined.saturating_sub(1),
                Tally::Invite => invited = invited.saturating_sub(1),
                Tally::Other => {}
            }
        }
        if let ChangeSide::Present(new) = &change.new {
            match tally(src, new)? {
                Tally::Join => joined = joined.saturating_add(1),
                Tally::Invite => invited = invited.saturating_add(1),
                Tally::Other => {}
            }
        }
    }
    let to_count = |n: i128| u64::try_from(n).map_err(|_| SummaryError::CountUnderflow);
    Ok(MembershipCounts {
        state_group_id: delta.target,
        version: ROOM_SUMMARY_VERSION,
        joined: to_count(joined)?,
        invited: to_count(invited)?,
    })
}

enum Tally {
    Join,
    Invite,
    Other,
}

fn tally<Id, C, S>(src: &mut S, id: &Id) -> Result<Tally, SummaryError<S::Error>>
where
    C: EventContent,
    S: SummarySource<Id, C>,
{
    let event = event(src, id)?;
    Ok(match event.content.get_membership() {
        Some("join") => Tally::Join,
        Some("invite") => Tally::Invite,
        _ => Tally::Other,
    })
}

fn get<Id, C, S>(src: &mut S, event_type: &EventType) -> Result<Option<Id>, SummaryError<S::Error>>
where
    S: SummarySource<Id, C>,
{
    src.get(event_type, "").map_err(SummaryError::Source)
}

fn event<Id, C, S>(src: &mut S, id: &Id) -> Result<Arc<LeanEvent<Id, C>>, SummaryError<S::Error>>
where
    S: SummarySource<Id, C>,
{
    src.event(id).map_err(SummaryError::Source)
}

fn display_field<Id, C, S>(
    src: &mut S,
    event_type: &EventType,
    field: &str,
) -> Result<Option<String>, SummaryError<S::Error>>
where
    C: EventContent,
    S: SummarySource<Id, C>,
{
    let Some(id) = get(src, event_type)? else {
        return Ok(None);
    };
    let event = event(src, &id)?;
    Ok(event
        .content
        .get_str_field(field)
        .filter(|value| !value.is_empty())
        .map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Value;
    use alloc::collections::BTreeMap;
    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec;

    type State = BTreeMap<(String, String), String>;

    /// In-memory source over a state map, counting event reads by type.
    #[derive(Default)]
    struct MemSource {
        state: State,
        events: BTreeMap<String, Arc<LeanEvent<String, Value>>>,
        lattices: BTreeMap<StateGroupId, LtHash>,
        member_event_reads: usize,
        event_reads: usize,
    }

    impl MemSource {
        fn add(&mut self, ty: &str, key: &str, id: &str, content: Value) {
            self.events.insert(
                id.to_string(),
                Arc::new(LeanEvent {
                    event_id: id.to_string(),
                    event_type: ty.to_string(),
                    state_key: Some(key.to_string()),
                    content,
                    ..Default::default()
                }),
            );
            self.state
                .insert((ty.to_string(), key.to_string()), id.to_string());
        }

        /// Records the current state's lattice and returns its group id.
        fn snapshot(&mut self) -> StateGroupId {
            let lattice = lattice_of(&self.state);
            let group = state_group_id_from_lthash(&lattice);
            self.lattices.insert(group, lattice);
            group
        }
    }

    fn lattice_of(state: &State) -> LtHash {
        let mut lattice = LtHash::default();
        for ((ty, key), id) in state {
            lattice.insert(ty, key, id);
        }
        lattice
    }

    impl SummarySource<String, Value> for MemSource {
        type Error = String;

        fn get(&mut self, ty: &EventType, key: &str) -> Result<Option<String>, String> {
            Ok(self
                .state
                .get(&(ty.as_str().to_string(), key.to_string()))
                .cloned())
        }

        fn for_each_member(&mut self, visit: &mut dyn FnMut(&str, &String)) -> Result<(), String> {
            for ((ty, key), id) in &self.state {
                if ty == "m.room.member" {
                    visit(key, id);
                }
            }
            Ok(())
        }

        fn event(&mut self, id: &String) -> Result<Arc<LeanEvent<String, Value>>, String> {
            let event = self
                .events
                .get(id)
                .cloned()
                .ok_or(format!("missing {id}"))?;
            self.event_reads = self.event_reads.saturating_add(1);
            if event.event_type == "m.room.member" {
                self.member_event_reads = self.member_event_reads.saturating_add(1);
            }
            Ok(event)
        }

        fn lattice(&mut self, group: &StateGroupId) -> Result<LtHash, String> {
            self.lattices
                .get(group)
                .copied()
                .ok_or_else(|| "unknown group".to_string())
        }
    }

    fn member(src: &mut MemSource, user: &str, id: &str, membership: &str) {
        src.add(
            "m.room.member",
            user,
            id,
            crate::json!({ "membership": membership }),
        );
    }

    fn room() -> MemSource {
        let mut src = MemSource::default();
        src.add("m.room.create", "", "$create", crate::json!({}));
        src.add("m.room.power_levels", "", "$pl", crate::json!({}));
        src.add(
            "m.room.join_rules",
            "",
            "$jr",
            crate::json!({ "join_rule": "invite" }),
        );
        src.add(
            "m.room.name",
            "",
            "$name",
            crate::json!({ "name": "Lobby" }),
        );
        src.add("m.room.topic", "", "$topic", crate::json!({ "topic": "" }));
        src.add(
            "m.room.encryption",
            "",
            "$enc",
            crate::json!({ "algorithm": "m.megolm.v1.aes-sha2" }),
        );
        member(&mut src, "@a:x", "$a1", "join");
        member(&mut src, "@b:x", "$b1", "join");
        member(&mut src, "@c:x", "$c1", "invite");
        member(&mut src, "@d:x", "$d1", "leave");
        src
    }

    /// `(event_type, state_key, Some((new_event_id, content)) | None to remove)`.
    type Change<'a> = (&'a str, &'a str, Option<(&'a str, Value)>);

    /// Applies `changes` to `src` (registering new events) and returns the
    /// complete delta from the previous group to the new one.
    fn step(src: &mut MemSource, base: StateGroupId, changes: &[Change<'_>]) -> StateDelta<String> {
        let mut out = Vec::new();
        for &(ty, key, ref new) in changes {
            let old = src
                .state
                .get(&(ty.to_string(), key.to_string()))
                .cloned()
                .map_or(ChangeSide::Absent, ChangeSide::Present);
            let new = if let Some((id, content)) = new {
                src.add(ty, key, id, content.clone());
                ChangeSide::Present((*id).to_string())
            } else {
                src.state.remove(&(ty.to_string(), key.to_string()));
                ChangeSide::Absent
            };
            out.push(StateChange {
                event_type: EventType::from(ty),
                state_key: key.to_string(),
                old,
                new,
            });
        }
        StateDelta {
            base,
            target: src.snapshot(),
            changes: out,
        }
    }

    /// `@c:x` accepts its invite.
    fn c_joins() -> Change<'static> {
        (
            "m.room.member",
            "@c:x",
            Some(("$c2", crate::json!({ "membership": "join" }))),
        )
    }

    /// The test room, its counts, and the delta applying `changes` to it.
    fn counted_step(changes: &[Change<'_>]) -> (MemSource, MembershipCounts, StateDelta<String>) {
        let mut src = room();
        let base = src.snapshot();
        let counts = derive_membership(base, &mut src).unwrap();
        let delta = step(&mut src, base, changes);
        (src, counts, delta)
    }

    #[test]
    fn singletons_resolve_display_fields_without_member_reads() {
        let mut src = room();
        let group = src.snapshot();
        let s = derive_singletons(group, &mut src).unwrap();
        assert_eq!(s.create, "$create");
        assert_eq!(s.power_levels.as_deref(), Some("$pl"));
        assert_eq!(s.join_rules.as_deref(), Some("$jr"));
        assert_eq!(s.name.as_deref(), Some("Lobby"));
        assert_eq!(s.topic, None, "empty topic reads as unset");
        assert_eq!(s.avatar_url, None);
        assert!(s.encrypted);
        assert_eq!(src.member_event_reads, 0);
        assert_eq!(src.event_reads, 3, "name, topic, encryption only");
    }

    #[test]
    fn singletons_require_create() {
        let mut src = room();
        src.state
            .remove(&("m.room.create".to_string(), String::new()));
        let group = src.snapshot();
        assert_eq!(
            derive_singletons(group, &mut src),
            Err(SummaryError::MissingCreate)
        );
    }

    #[test]
    fn membership_counts_join_and_invite_only() {
        let mut src = room();
        let group = src.snapshot();
        let counts = derive_membership(group, &mut src).unwrap();
        assert_eq!((counts.joined, counts.invited), (2, 1));
        assert_eq!(counts.state_group_id, group);
    }

    #[test]
    fn chained_updates_match_full_derivation_at_every_step() {
        let mut src = room();
        let mut group = src.snapshot();
        let mut counts = derive_membership(group, &mut src).unwrap();
        let steps: Vec<Vec<Change<'_>>> = vec![
            // invite -> join, plus an unrelated topic change
            vec![
                c_joins(),
                (
                    "m.room.topic",
                    "",
                    Some(("$topic2", crate::json!({ "topic": "hi" }))),
                ),
            ],
            // join -> leave
            vec![(
                "m.room.member",
                "@a:x",
                Some(("$a2", crate::json!({ "membership": "leave" }))),
            )],
            // new invite, and a member key removed outright
            vec![
                (
                    "m.room.member",
                    "@e:x",
                    Some(("$e1", crate::json!({ "membership": "invite" }))),
                ),
                ("m.room.member", "@b:x", None),
            ],
            // leave -> join, ban
            vec![
                (
                    "m.room.member",
                    "@d:x",
                    Some(("$d2", crate::json!({ "membership": "join" }))),
                ),
                (
                    "m.room.member",
                    "@e:x",
                    Some(("$e2", crate::json!({ "membership": "ban" }))),
                ),
            ],
        ];
        for changes in steps {
            let delta = step(&mut src, group, &changes);
            counts = update_membership(&counts, &delta, &mut src).unwrap();
            let full = derive_membership(delta.target, &mut src).unwrap();
            assert_eq!(counts, full);
            group = delta.target;
        }
        assert_eq!((counts.joined, counts.invited), (2, 0));
    }

    #[test]
    fn incomplete_or_wrong_deltas_are_rejected_before_reading_events() {
        let (mut src, counts, delta) = counted_step(&[
            c_joins(),
            (
                "m.room.name",
                "",
                Some(("$name2", crate::json!({ "name": "New" }))),
            ),
        ]);

        // Members-only: drops the name change.
        let mut members_only = delta.clone();
        members_only
            .changes
            .retain(|c| c.event_type == EventType::RoomMember);
        // Lies about the old side.
        let mut false_absent = delta.clone();
        false_absent.changes[0].old = ChangeSide::Absent;

        src.event_reads = 0;
        for bad in [members_only, false_absent] {
            assert!(matches!(
                update_membership(&counts, &bad, &mut src),
                Err(SummaryError::DeltaMismatch { .. })
            ));
        }
        assert_eq!(src.event_reads, 0);
    }

    #[test]
    fn non_canonical_deltas_and_wrong_parents_are_rejected() {
        let (mut src, counts, delta) = counted_step(&[c_joins()]);

        let mut duplicated = delta.clone();
        duplicated.changes.push(delta.changes[0].clone());
        assert!(matches!(
            update_membership(&counts, &duplicated, &mut src),
            Err(SummaryError::DuplicateKey { .. })
        ));

        let mut no_op = delta.clone();
        no_op.changes[0].new = no_op.changes[0].old.clone();
        assert!(matches!(
            update_membership(&counts, &no_op, &mut src),
            Err(SummaryError::EmptyChange { .. })
        ));

        let stranger = MembershipCounts {
            state_group_id: [7; 32],
            ..counts
        };
        assert!(matches!(
            update_membership(&stranger, &delta, &mut src),
            Err(SummaryError::ParentMismatch { .. })
        ));

        let stale = MembershipCounts {
            version: ROOM_SUMMARY_VERSION + 1,
            ..counts
        };
        assert!(matches!(
            update_membership(&stale, &delta, &mut src),
            Err(SummaryError::ParentVersionMismatch { .. })
        ));
    }

    #[test]
    fn wrong_parent_counts_underflow_instead_of_wrapping() {
        let mut src = room();
        let base = src.snapshot();
        let delta = step(
            &mut src,
            base,
            &[(
                "m.room.member",
                "@a:x",
                Some(("$a2", crate::json!({ "membership": "leave" }))),
            )],
        );
        let bogus = MembershipCounts {
            state_group_id: base,
            version: ROOM_SUMMARY_VERSION,
            joined: 0,
            invited: 0,
        };
        assert_eq!(
            update_membership(&bogus, &delta, &mut src),
            Err(SummaryError::CountUnderflow)
        );
    }

    #[test]
    fn derivation_does_not_mutate_the_source() {
        let mut src = room();
        let group = src.snapshot();
        let (state, lattices) = (src.state.clone(), src.lattices.clone());
        derive_singletons(group, &mut src).unwrap();
        derive_membership(group, &mut src).unwrap();
        assert_eq!(src.state, state);
        assert_eq!(src.lattices, lattices);
    }
}
