//! Flat Matrix state-HAMT root record.
//!
//! Every distinct room state set is persisted as an `MTHR` root record whose
//! internal nodes are the generic [`MTHN`](crate::hamt::HAMT_NODE_MAGIC)
//! records built by [`build_hamt_root_handle`](crate::hamt::build_hamt_root_handle).
//!
//! Leaves are self-describing: the key is `(EventType, String)`, encoded by the
//! dense [`HamtCodec`](crate::hamt::HamtCodec) as length-prefixed event-type and
//! state-key bytes, and the value is a 32-byte event hash (see
//! [`state_hamt_event_id`]). In room versions 3 and later an event id already
//! is a hash, `$` followed by the unpadded base64 encoding of 32 bytes, and the
//! value is those bytes. Room versions 1 and 2 use opaque event ids, so the
//! value is a domain-separated BLAKE3 hash of the id string. Either way every
//! server holds the same bytes, with no interning table or collision handling.
//! Only the v3+ form can be turned back into an id; a caller storing v1/v2
//! leaves keeps its own hash-to-id index.
//!
//! The state-group id is the unkeyed `LtHash` digest of the logical
//! `(event_type, state_key, event_id)` strings, so it is the only portable
//! identity of a state set. Keeping the leaf key as strings also lets a reader
//! check the retained lattice against the leaves and serve type-scoped reads
//! without a reverse index.
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
//! See [`crate::hamt`] for the deferred lazy-resolver and typed-root notes.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::incremental::LtHash;

use super::{state_group_id_from_lthash, StateGroupId, StructuralHash, HAMT_ROOT_MAGIC};

/// Wire version of the flat `MTHR` root record.
pub const STATE_HAMT_ROOT_VERSION: u8 = 0x01;

/// Byte length of the retained 1024-lane state lattice.
pub const STATE_HAMT_LATTICE_BYTES: usize = 2048;

/// Canonical state-HAMT leaf key: event type plus state key.
///
/// The dense codec writes both as length-prefixed UTF-8, so the key is
/// self-describing and byte-identical across servers.
pub type StateHamtKey = (crate::basespec::event_types::EventType, String);

/// A state-HAMT leaf value: the 32-byte hash that identifies an event.
///
/// Built by [`state_hamt_event_id`]: the reference hash a room-v3+ event id
/// encodes, or a hash of the opaque id string in room versions 1 and 2.
pub type StateHamtEventId = [u8; 32];

/// BLAKE3 key-derivation context for hashing opaque (room v1/v2) event ids.
///
/// The domain separation keeps these hashes apart from v3+ reference hashes,
/// which are SHA-256 digests of redacted events.
const OPAQUE_EVENT_ID_CONTEXT: &str = "rezzy state-hamt opaque event id";

/// Why an event id has no state-HAMT leaf value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateHamtEventIdError {
    /// The room version's event ids are hashes, but this id is not `$`
    /// followed by the canonical unpadded base64 of 32 bytes.
    NotAHashId,
}

impl fmt::Display for StateHamtEventIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAHashId => {
                f.write_str("event id is not the base64 hash its room version requires")
            }
        }
    }
}

impl core::error::Error for StateHamtEventIdError {}

/// The state-HAMT leaf value for `event_id` in a room of `room_version`.
///
/// Room versions 1 and 2 hash the opaque id string. Later versions decode the
/// reference hash the id carries, with the room version's alphabet (standard
/// for v3, URL-safe after). Decoding is strict, so each hash has exactly one
/// id, and an id that does not decode is an error, never hashed instead: its
/// leaf could not be turned back into an id.
///
/// # Errors
/// Returns [`StateHamtEventIdError::NotAHashId`] when a v3+ id does not decode
/// to 32 bytes.
pub fn state_hamt_event_id(
    event_id: &str,
    room_version: &str,
) -> Result<StateHamtEventId, StateHamtEventIdError> {
    use base64::Engine as _;

    if crate::has_opaque_event_ids(room_version) {
        return Ok(blake3::derive_key(
            OPAQUE_EVENT_ID_CONTEXT,
            event_id.as_bytes(),
        ));
    }
    let encoded = event_id
        .strip_prefix('$')
        .filter(|encoded| encoded.len() == 43)
        .ok_or(StateHamtEventIdError::NotAHashId)?;
    // 43 characters estimate to 33 bytes; a 32-byte hash fills all but one.
    let mut decoded = [0_u8; 33];
    let length = crate::basespec::rezzy_types::hash_base64_engine(room_version)
        .decode_slice(encoded, &mut decoded)
        .map_err(|_| StateHamtEventIdError::NotAHashId)?;
    decoded
        .get(..length)
        .and_then(|bytes| StateHamtEventId::try_from(bytes).ok())
        .ok_or(StateHamtEventIdError::NotAHashId)
}

/// The event id a state-HAMT leaf value stands for, when it can be recovered.
///
/// Returns `None` in room versions 1 and 2, whose values are one-way hashes.
#[must_use]
pub fn state_hamt_event_id_string(value: &StateHamtEventId, room_version: &str) -> Option<String> {
    use base64::Engine as _;

    if crate::has_opaque_event_ids(room_version) {
        return None;
    }
    let mut encoded = [0_u8; 43];
    let length = crate::basespec::rezzy_types::hash_base64_engine(room_version)
        .encode_slice(value, &mut encoded)
        .ok()?;
    let mut event_id = String::with_capacity(44);
    event_id.push('$');
    event_id.push_str(core::str::from_utf8(encoded.get(..length)?).ok()?);
    Some(event_id)
}

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
        let value: StateHamtEventId = [7_u8; 32];

        let mut encoded = Vec::new();
        key.encode_hamt(&mut encoded);
        value.encode_hamt(&mut encoded);

        let mut cursor = 0;
        let decoded_key = <(EventType, String)>::decode_hamt(&encoded, &mut cursor).unwrap();
        let decoded_value = <[u8; 32]>::decode_hamt(&encoded, &mut cursor).unwrap();

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
            4 + event_type.len() + 4 + state_key.len() + 32
        );
    }

    #[test]
    fn state_leaf_shape_builds_and_looks_up() {
        use crate::basespec::event_types::EventType;

        let key: StateHamtKey = (
            EventType::from("m.room.member"),
            "@alice:example.org".to_string(),
        );
        let value: StateHamtEventId = [3_u8; 32];
        let (_, root) = crate::hamt::build_hamt_root_handle(
            b"!room:example.org",
            &LtHash::ZERO,
            vec![(key.clone(), value)],
        )
        .expect("build");

        assert_eq!(root.get(b"!room:example.org", &key), Some(&value));
    }

    #[test]
    fn hash_event_ids_round_trip_with_their_room_version_alphabet() {
        // Bytes whose encoding uses both alphabet-specific characters.
        let mut bytes = [0_u8; 32];
        bytes[0] = 0xfb;
        bytes[1] = 0xff;
        bytes[2] = 0xbf;

        for (room_version, id) in [
            ("3", "$+/+/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            ("10", "$-_-_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        ] {
            assert_eq!(state_hamt_event_id(id, room_version), Ok(bytes));
            assert_eq!(
                state_hamt_event_id_string(&bytes, room_version).as_deref(),
                Some(id)
            );
        }
        // The other version's alphabet is not a hash id here.
        assert_eq!(
            state_hamt_event_id("$-_-_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "3"),
            Err(StateHamtEventIdError::NotAHashId)
        );
    }

    #[test]
    fn malformed_hash_event_ids_are_rejected() {
        for id in [
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "$AAAA",
            "$abc:example.org",
            // Non-zero trailing bits: a second spelling of the same bytes.
            "$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB",
        ] {
            assert_eq!(
                state_hamt_event_id(id, "10"),
                Err(StateHamtEventIdError::NotAHashId),
                "{id}"
            );
        }
    }

    #[test]
    fn opaque_event_ids_hash_one_way() {
        let id = "$abc123:example.org";
        let value = state_hamt_event_id(id, "1").unwrap();
        assert_eq!(state_hamt_event_id(id, "2"), Ok(value));
        assert_ne!(state_hamt_event_id("$abc124:example.org", "1"), Ok(value));
        assert_eq!(state_hamt_event_id_string(&value, "1"), None);
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
