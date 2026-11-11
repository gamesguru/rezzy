//! Flat Matrix state-HAMT root record, built on the generic [`crate::hamt`].
//!
//! Every distinct room state set is persisted as an `MTHR` root record whose
//! internal nodes are the generic [`MTHN`](crate::hamt::HAMT_NODE_MAGIC)
//! records built by [`build_hamt_root_handle`](crate::hamt::build_hamt_root_handle).
//!
//! Leaves are self-describing: the key is `(EventType, String)`, encoded by the
//! dense [`HamtCodec`](crate::hamt::HamtCodec) as length-prefixed event-type and
//! state-key bytes, and the value is the event ID string. Storing the ID itself
//! works for every room version, including v1/v2 opaque IDs, needs no interning
//! table or hash-to-ID index, and keeps the lattice checkable from the leaves.
//!
//! The state-group id is the unkeyed `LtHash` digest of the logical
//! `(event_type, state_key, event_id)` strings, so it is the only portable
//! identity of a state set. Keeping the leaf key as strings also lets a reader
//! serve type-scoped reads without a reverse index.
//!
//! Whether node bytes and structural hashes match across servers depends on the
//! caller's structural key: one derived from the room id alone gives identical
//! trees for identical states, while one keyed by a per-server secret makes
//! them local to that server.
//!
//! The current flat layout is:
//!
//! ```text
//! "MTHR" (4)
//! version 0x01 (1)
//! room-prefix length (u16 BE)
//! room prefix
//! room-id length (u16 BE)
//! UTF-8 room id
//! root structural hash (32)
//! lattice (2048 bytes: 1024 u16 lanes, little-endian)
//! ```
//!
//! # Read-path design notes
//!
//! [`PersistedHamtReader`](crate::hamt::PersistedHamtReader) and this module
//! are the supported read side for flat Matrix state HAMTs. Two adjacent
//! changes are deliberately deferred.
//!
//! ## Lazy resolver entry point
//!
//! State-resolution entry points (`resolve_state_maps_cached`,
//! `resolve_iterative_sort*`) take `&HashMap<Id, LeanEvent>`. An importer that
//! walks only the new events of a batch therefore cannot resolve a merge whose
//! auth closure reaches outside that batch: it reports an incomplete
//! resolution and falls back to the first-parent rule. The enabling change is
//! a resolver variant that takes a source/callback instead of a map, so the
//! caller materializes an auth-context event on first read and still reports a
//! missing id as incomplete. That is a separate, correctness-sensitive change;
//! it should be driven by a profile showing merges dominate.
//!
//! ## Typed state roots
//!
//! The state-root codec uses a flat layout. A typed layout could support
//! type-scoped reads, but adds a directory lookup to point reads, rewrites the
//! directory when a subtree changes, and grows root records. Full-state reads
//! and bulk imports do not benefit from it, so typed roots should be driven by
//! measured type-scoped read demand. The decision record and constraints are in
//! `docs/2026-11-11-typed-hamt-roots.md`.
//!
//! Portable node hashes need canonical, cross-server leaf encodings (the
//! `(EventType, String)` key and event-ID strings here qualify; homeserver-local
//! short IDs do not) and the same room-scoped structural key on every server.
//! Routing is already canonical: the HAMT routes by encoded key bytes.
//!
//! Public routing does not make prefix grinding unbounded. With 32-way nodes a
//! shared path of `d` levels costs an attacker about `2^(5d)` attempts per key
//! (four levels is instant, ten is impractical at scale), each ground key still
//! needs a state event subject to power-level and rate limits, and every extra
//! level adds only one node to the targeted path. A depth cap bounds the worst
//! case, and an epoch-rotated structural key invalidates precomputed collisions
//! at the cost of rebuilding the tree. Servers that only compare state groups
//! can keep a per-server structural key and use the portable `state_group_id`.
//!
//! Independently, the `LtHash` identity (`BLAKE3(lattice)`) and the resolution
//! algorithms do not change with the root layout: cross-server state-group
//! identity and every stored group depend on the `LtHash` of the logical entries.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::incremental::LtHash;

use crate::hamt::{state_group_id_from_lthash, StateGroupId, StructuralHash, HAMT_ROOT_MAGIC};

/// Wire version of the flat `MTHR` root record.
pub const STATE_HAMT_ROOT_VERSION: u8 = 0x01;

/// Byte length of the retained 1024-lane state lattice.
pub const STATE_HAMT_LATTICE_BYTES: usize = 2048;

/// Canonical state-HAMT leaf key: event type plus state key.
///
/// The dense codec writes both as length-prefixed UTF-8, so the key is
/// self-describing and byte-identical across servers.
pub type StateHamtKey = (crate::basespec::event_types::EventType, String);

/// A decoded `MTHR` root record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateHamtRoot {
    /// Room-scoped prefix baked into every node's structural key.
    pub prefix: Vec<u8>,
    /// The room id the records belong to.
    pub room_id: String,
    /// Structural hash of the root `MTHN` node.
    pub root_hash: StructuralHash,
    /// Unkeyed `LtHash` digest of the state set (the state-group id).
    pub state_group_id: StateGroupId,
    /// The retained lattice. `state_group_id` is `lattice.digest()`.
    pub lattice: LtHash,
}

/// Why an `MTHR` root record could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateHamtRootError {
    /// The room prefix is longer than the `u16` length field allows.
    PrefixTooLong,
    /// The room id is longer than the `u16` length field allows.
    RoomIdTooLong,
    /// The buffer ended before a declared field was complete.
    Truncated,
    /// The record does not start with `MTHR`.
    BadMagic,
    /// The record uses a version this reader does not know.
    UnsupportedVersion(u8),
    /// The room-id bytes are not valid UTF-8.
    InvalidRoomId,
    /// The trailing lattice is not exactly 2048 bytes.
    InvalidLattice,
}

impl fmt::Display for StateHamtRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixTooLong => f.write_str("state HAMT room prefix exceeds u16 length"),
            Self::RoomIdTooLong => f.write_str("state HAMT room id exceeds u16 length"),
            Self::Truncated => f.write_str("state HAMT root record is truncated"),
            Self::BadMagic => f.write_str("state HAMT root record does not start with MTHR"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported state HAMT root version {version:#04x}")
            }
            Self::InvalidRoomId => f.write_str("state HAMT root room id is not UTF-8"),
            Self::InvalidLattice => {
                f.write_str("state HAMT root lattice is not exactly 2048 bytes")
            }
        }
    }
}

impl core::error::Error for StateHamtRootError {}

/// Encode an `MTHR\x01` root record.
///
/// # Errors
/// Returns [`StateHamtRootError::PrefixTooLong`] or
/// [`StateHamtRootError::RoomIdTooLong`] when either length does not fit the
/// wire format's `u16` field.
pub fn encode_state_hamt_root(
    prefix: &[u8],
    room_id: &str,
    root_hash: &StructuralHash,
    lattice: &LtHash,
) -> Result<Vec<u8>, StateHamtRootError> {
    let prefix_len = u16::try_from(prefix.len()).map_err(|_| StateHamtRootError::PrefixTooLong)?;
    let room_id_len =
        u16::try_from(room_id.len()).map_err(|_| StateHamtRootError::RoomIdTooLong)?;

    let mut out = Vec::with_capacity(
        HAMT_ROOT_MAGIC
            .len()
            .saturating_add(1)
            .saturating_add(2)
            .saturating_add(prefix.len())
            .saturating_add(2)
            .saturating_add(room_id.len())
            .saturating_add(32)
            .saturating_add(STATE_HAMT_LATTICE_BYTES),
    );
    out.extend_from_slice(HAMT_ROOT_MAGIC);
    out.push(STATE_HAMT_ROOT_VERSION);
    out.extend_from_slice(&prefix_len.to_be_bytes());
    out.extend_from_slice(prefix);
    out.extend_from_slice(&room_id_len.to_be_bytes());
    out.extend_from_slice(room_id.as_bytes());
    out.extend_from_slice(root_hash);
    for lane in lattice.as_ref() {
        out.extend_from_slice(&lane.to_le_bytes());
    }
    Ok(out)
}

/// Decode an `MTHR\x01` root record.
///
/// # Errors
/// Returns [`StateHamtRootError`] when the magic, version, declared lengths,
/// UTF-8 room id, or trailing lattice are invalid. Trailing bytes after the
/// lattice are rejected: the record is fixed-width.
pub fn decode_state_hamt_root(bytes: &[u8]) -> Result<StateHamtRoot, StateHamtRootError> {
    if bytes.get(..HAMT_ROOT_MAGIC.len()) != Some(HAMT_ROOT_MAGIC.as_slice()) {
        return Err(StateHamtRootError::BadMagic);
    }
    let mut cursor = HAMT_ROOT_MAGIC.len();
    let version = *bytes.get(cursor).ok_or(StateHamtRootError::Truncated)?;
    cursor = cursor.saturating_add(1);
    if version != STATE_HAMT_ROOT_VERSION {
        return Err(StateHamtRootError::UnsupportedVersion(version));
    }

    let prefix = read_u16_prefixed(bytes, &mut cursor)?.to_vec();
    let room_id_bytes = read_u16_prefixed(bytes, &mut cursor)?;
    let room_id = core::str::from_utf8(room_id_bytes)
        .map_err(|_| StateHamtRootError::InvalidRoomId)?
        .to_string();

    let root_hash_end = cursor
        .checked_add(32)
        .ok_or(StateHamtRootError::Truncated)?;
    let root_hash_bytes = bytes
        .get(cursor..root_hash_end)
        .ok_or(StateHamtRootError::Truncated)?;
    let mut root_hash = [0_u8; 32];
    root_hash.copy_from_slice(root_hash_bytes);
    cursor = root_hash_end;

    let lattice_bytes = bytes.get(cursor..).ok_or(StateHamtRootError::Truncated)?;
    let lattice = LtHash::from_bytes(lattice_bytes).ok_or(StateHamtRootError::InvalidLattice)?;

    Ok(StateHamtRoot {
        prefix,
        room_id,
        root_hash,
        state_group_id: state_group_id_from_lthash(&lattice),
        lattice,
    })
}

fn read_u16_prefixed<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
) -> Result<&'a [u8], StateHamtRootError> {
    let length_end = cursor.checked_add(2).ok_or(StateHamtRootError::Truncated)?;
    let length_bytes = bytes
        .get(*cursor..length_end)
        .ok_or(StateHamtRootError::Truncated)?;
    let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
    *cursor = length_end;
    let end = cursor
        .checked_add(length)
        .ok_or(StateHamtRootError::Truncated)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or(StateHamtRootError::Truncated)?;
    *cursor = end;
    Ok(value)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use alloc::vec;

    fn lattice() -> LtHash {
        let mut lattice = LtHash::ZERO;
        lattice.insert("m.room.create", "", "$create");
        lattice.insert("m.room.member", "@alice:example.org", "$join");
        lattice
    }

    #[test]
    fn root_record_round_trips() {
        let lattice = lattice();
        let root_hash = [7_u8; 32];
        let bytes = encode_state_hamt_root(b"prefix!!", "!room:example.org", &root_hash, &lattice)
            .expect("encode");
        assert_eq!(
            bytes.len(),
            7 + b"prefix!!".len() + 2 + "!room:example.org".len() + 32 + 2048
        );
        let decoded = decode_state_hamt_root(&bytes).expect("decode");
        assert_eq!(decoded.prefix, b"prefix!!");
        assert_eq!(decoded.room_id, "!room:example.org");
        assert_eq!(decoded.root_hash, root_hash);
        assert_eq!(decoded.lattice, lattice);
        assert_eq!(decoded.state_group_id, lattice.digest());
    }

    #[test]
    fn root_record_is_the_documented_layout() {
        let lattice = LtHash::ZERO;
        let bytes = encode_state_hamt_root(b"ab", "!r", &[9_u8; 32], &lattice).expect("encode");
        assert_eq!(&bytes[..5], b"MTHR\x01");
        assert_eq!(&bytes[5..7], &2_u16.to_be_bytes());
        assert_eq!(&bytes[7..9], b"ab");
        assert_eq!(&bytes[9..11], &2_u16.to_be_bytes());
        assert_eq!(&bytes[11..13], b"!r");
        assert_eq!(&bytes[13..45], &[9_u8; 32]);
        assert_eq!(&bytes[45..], &[0_u8; 2048][..]);
    }

    #[test]
    fn root_record_rejects_bad_input() {
        assert_eq!(
            decode_state_hamt_root(b"MTHX\x01"),
            Err(StateHamtRootError::BadMagic)
        );
        assert!(matches!(
            decode_state_hamt_root(b"MTHR\x02"),
            Err(StateHamtRootError::UnsupportedVersion(2))
        ));

        let bytes = encode_state_hamt_root(b"", "!r", &[0_u8; 32], &LtHash::ZERO).unwrap();
        assert_eq!(
            decode_state_hamt_root(&bytes[..bytes.len() - 1]),
            Err(StateHamtRootError::InvalidLattice)
        );
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            decode_state_hamt_root(&trailing),
            Err(StateHamtRootError::InvalidLattice)
        );
    }

    #[test]
    fn leaf_key_and_event_id_codec_round_trip() {
        use crate::basespec::event_types::EventType;
        use crate::hamt::HamtCodec;

        let event_type = "m.room.member";
        let state_key = "@alice:example.org";
        let key: StateHamtKey = (EventType::from(event_type), state_key.to_string());
        let value = "$event:example.org".to_string();

        let mut encoded = Vec::new();
        key.encode_hamt(&mut encoded);
        value.encode_hamt(&mut encoded);

        let mut cursor = 0;
        let decoded_key = <(EventType, String)>::decode_hamt(&encoded, &mut cursor).unwrap();
        let decoded_value = String::decode_hamt(&encoded, &mut cursor).unwrap();

        assert_eq!(decoded_key, key);
        assert_eq!(decoded_value, value);
        assert_eq!(cursor, encoded.len());
        // Length-prefixed binary, not JSON.
        assert_eq!(
            &encoded[..4],
            &u32::try_from(event_type.len()).unwrap().to_le_bytes()
        );
        assert_eq!(
            encoded.len(),
            4 + event_type.len() + 4 + state_key.len() + 4 + value.len()
        );
    }

    #[test]
    fn state_leaf_shape_builds_and_looks_up() {
        use crate::basespec::event_types::EventType;

        let key: StateHamtKey = (
            EventType::from("m.room.member"),
            "@alice:example.org".to_string(),
        );
        let value = "$member:example.org".to_string();
        let (_, root) = crate::hamt::build_hamt_root_handle(
            b"!room:example.org",
            &LtHash::ZERO,
            vec![(key.clone(), value.clone())],
        )
        .expect("build");

        assert_eq!(root.get(b"!room:example.org", &key), Some(&value));
    }

    #[test]
    fn overlong_fields_are_rejected() {
        let long_room = "!".repeat(usize::from(u16::MAX) + 1);
        assert_eq!(
            encode_state_hamt_root(b"", &long_room, &[0_u8; 32], &LtHash::ZERO),
            Err(StateHamtRootError::RoomIdTooLong)
        );
        assert_eq!(
            encode_state_hamt_root(
                &vec![0_u8; usize::from(u16::MAX) + 1],
                "!r",
                &[0_u8; 32],
                &LtHash::ZERO
            ),
            Err(StateHamtRootError::PrefixTooLong)
        );
    }
}
