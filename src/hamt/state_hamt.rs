//! Flat Matrix state-HAMT root record and leaf-key codec.
//!
//! Every distinct room state set is persisted as an `MTHR` root record whose
//! internal nodes are the generic [`MTHN`](crate::hamt::HAMT_NODE_MAGIC)
//! records built by [`build_hamt_root_handle`](crate::hamt::build_hamt_root_handle).
//! The state-group id is the unkeyed `LtHash` digest of the logical
//! `(event_type, state_key, event_id)` entries, and the retained 2048-byte
//! lattice lets a reader verify a root without rebuilding the state.
//!
//! The layout is byte-compatible with Synapse's `_encode_state_hamt_root` and
//! its sithnapse mirror:
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
//! Leaf keys are the JSON encoding of `(event_type, state_key)` — for example
//! `["m.room.member","@alice:example.org"]` — matching the `serde_json`
//! encoding Synapse writes.
//!
//! See [`crate::hamt`] for the deferred lazy-resolver and typed-root notes.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::{self, Write as _};

use crate::incremental::LtHash;

use super::{state_group_id_from_lthash, StateGroupId, StructuralHash, HAMT_ROOT_MAGIC};

/// Wire version of the flat `MTHR` root record.
pub const STATE_HAMT_ROOT_VERSION: u8 = 0x01;

/// Byte length of the retained 1024-lane state lattice.
pub const STATE_HAMT_LATTICE_BYTES: usize = 2048;

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

/// Why a state-HAMT leaf key could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateHamtLeafKeyError {
    /// The key is not valid JSON.
    InvalidJson,
    /// The key is not a JSON array.
    NotArray,
    /// The key does not have exactly two elements.
    WrongArity,
    /// One or both elements are not strings.
    NonString,
}

impl fmt::Display for StateHamtLeafKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => f.write_str("state HAMT leaf key is not valid JSON"),
            Self::NotArray => f.write_str("state HAMT leaf key is not a JSON array"),
            Self::WrongArity => f.write_str("state HAMT leaf key is not a two-element array"),
            Self::NonString => f.write_str("state HAMT leaf key elements must be strings"),
        }
    }
}

impl core::error::Error for StateHamtLeafKeyError {}

/// Encode the logical `(event_type, state_key)` as a state-HAMT leaf key.
///
/// This emits the `serde_json` tuple encoding Synapse uses, including its
/// control-character escapes, so the bytes are portable across implementations.
#[must_use]
pub fn state_hamt_leaf_key(event_type: &str, state_key: &str) -> String {
    let mut out = String::new();
    out.push('[');
    push_json_string(&mut out, event_type);
    out.push(',');
    push_json_string(&mut out, state_key);
    out.push(']');
    out
}

/// Decode a state-HAMT leaf key into `(event_type, state_key)`.
///
/// # Errors
/// Returns [`StateHamtLeafKeyError`] when the key is not a two-string JSON
/// array.
pub fn decode_state_hamt_leaf_key(key: &str) -> Result<(String, String), StateHamtLeafKeyError> {
    let value = crate::json::Value::parse(key).map_err(|_| StateHamtLeafKeyError::InvalidJson)?;
    let crate::json::Value::Array(elements) = value else {
        return Err(StateHamtLeafKeyError::NotArray);
    };
    let [event_type, state_key] = elements.as_slice() else {
        return Err(StateHamtLeafKeyError::WrongArity);
    };
    let (Some(event_type), Some(state_key)) = (event_type.as_str(), state_key.as_str()) else {
        return Err(StateHamtLeafKeyError::NonString);
    };
    Ok((event_type.to_string(), state_key.to_string()))
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
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
    fn leaf_keys_match_the_tuple_encoding() {
        assert_eq!(
            state_hamt_leaf_key("m.room.member", "@alice:example.org"),
            r#"["m.room.member","@alice:example.org"]"#
        );
        let awkward = "a\"b\\c\nd\te\u{01}é";
        let key = state_hamt_leaf_key(awkward, awkward);
        assert_eq!(
            decode_state_hamt_leaf_key(&key).expect("decode"),
            (awkward.to_string(), awkward.to_string())
        );
    }

    #[test]
    fn leaf_key_decoder_reports_shape_errors() {
        assert_eq!(
            decode_state_hamt_leaf_key("not json"),
            Err(StateHamtLeafKeyError::InvalidJson)
        );
        assert_eq!(
            decode_state_hamt_leaf_key(r#"{"a":1}"#),
            Err(StateHamtLeafKeyError::NotArray)
        );
        assert_eq!(
            decode_state_hamt_leaf_key(r"[1,2]"),
            Err(StateHamtLeafKeyError::NonString)
        );
        assert_eq!(
            decode_state_hamt_leaf_key(r#"["only"]"#),
            Err(StateHamtLeafKeyError::WrongArity)
        );
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
