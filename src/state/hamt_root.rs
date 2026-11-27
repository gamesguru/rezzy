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
        );

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

/// Golden vectors for the flat state-HAMT format.
///
/// A fixed two-entry state set pins the persisted node bytes, the structural
/// `root_hash`, the portable `state_group_id`, and the BLAKE3 digest of the
/// `MTHR` record. Changing the leaf codec, routing, structural-hash preimage or
/// root-record layout must change these constants deliberately; an accidental
/// change should fail here.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod golden_vectors {
    use super::*;
    use crate::basespec::event_types::EventType;
    use crate::hamt::codec::PersistedInternalNode;
    use crate::hamt::{
        HAMT_CODEC_VERSION, HAMT_NODE_MAGIC, HAMT_ROOT_MAGIC, HAMT_ROUTING_VERSION,
        HAMT_WIRE_VERSION,
    };
    use alloc::{
        string::{String, ToString},
        vec,
        vec::Vec,
    };

    const GOLDEN_ROOM_ID: &str = "!golden:example.org";
    const GOLDEN_NODE_BYTES_HEX: &str =
        "4d54484e01100000010000000002000000000000000d0000006d2e726f6f6d2e6372656174650000000013000000246372656174653a6578616d706c652e6f72670d0000006d2e726f6f6d2e6d656d6265721200000040616c6963653a6578616d706c652e6f726711000000246a6f696e3a6578616d706c652e6f7267";
    const GOLDEN_ROOT_HASH_HEX: &str =
        "815a6dadafb5aed3195b8a9a5b24985e312fb940d8e754866c744d90c83e6d15";
    const GOLDEN_STATE_GROUP_ID_HEX: &str =
        "8716f8f25928d52ddf2a97866c257e27abcafff216dc758f53b294033f3c54eb";
    const GOLDEN_MTHR_LEN: usize = 2127;
    const GOLDEN_MTHR_BLAKE3_HEX: &str =
        "04e50c14f9b9cbad1807353a07f4fd53b5d71b377be154eb31a4edfc4fae68c7";

    fn hex(bytes: &[u8]) -> String {
        use core::fmt::Write as _;
        let mut out = String::with_capacity(bytes.len().saturating_mul(2));
        for byte in bytes {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    fn golden_entries() -> Vec<(StateHamtKey, String)> {
        vec![
            (
                (EventType::from("m.room.create"), String::new()),
                "$create:example.org".to_string(),
            ),
            (
                (
                    EventType::from("m.room.member"),
                    "@alice:example.org".to_string(),
                ),
                "$join:example.org".to_string(),
            ),
        ]
    }

    fn golden_lattice(entries: &[(StateHamtKey, String)]) -> LtHash {
        let mut lattice = LtHash::ZERO;
        for (key, value) in entries {
            lattice.insert(key.0.as_str(), &key.1, value);
        }
        lattice
    }

    fn golden_tree() -> (
        crate::hamt::RootHandle,
        alloc::sync::Arc<crate::hamt::HamtNode<StateHamtKey, String>>,
        LtHash,
    ) {
        let entries = golden_entries();
        let lattice = golden_lattice(&entries);
        let (handle, root) =
            crate::hamt::build_hamt_root_handle(GOLDEN_ROOM_ID.as_bytes(), &lattice, entries);
        (handle, root, lattice)
    }

    #[test]
    fn golden_node_root_and_record_vectors() {
        let (handle, root, lattice) = golden_tree();

        // The two golden leaves diverge at the root, so the whole tree is one
        // inline node.
        assert_eq!(root.nodemap, 0);
        assert_eq!(root.children.len(), 0);
        assert_eq!(root.leaves.len(), 2);

        let persisted = PersistedInternalNode::from(root.as_ref());
        let node_bytes = persisted.encode_v1();
        assert_eq!(hex(&node_bytes), GOLDEN_NODE_BYTES_HEX);
        assert_eq!(hex(&handle.structural_hash), GOLDEN_ROOT_HASH_HEX);
        assert_eq!(hex(&handle.state_group_id), GOLDEN_STATE_GROUP_ID_HEX);
        assert_eq!(handle.state_group_id, lattice.digest());

        let record = encode_state_hamt_root(
            GOLDEN_ROOM_ID.as_bytes(),
            GOLDEN_ROOM_ID,
            &handle.structural_hash,
            &lattice,
        )
        .expect("golden record must encode");
        assert_eq!(record.len(), GOLDEN_MTHR_LEN);
        assert_eq!(&record[..5], b"MTHR\x01");
        let digest = *blake3::hash(&record).as_bytes();
        assert_eq!(hex(&digest), GOLDEN_MTHR_BLAKE3_HEX);
        // The record digest commits to the full 2048-byte lattice, which is
        // separately identified by the state-group id.
        assert_eq!(hex(&lattice.digest()), GOLDEN_STATE_GROUP_ID_HEX);
    }

    #[test]
    fn golden_node_decodes_and_reencodes() {
        let (handle, root, _lattice) = golden_tree();
        let bytes = PersistedInternalNode::from(root.as_ref()).encode_v1();

        let decoded = PersistedInternalNode::<StateHamtKey, String>::decode_v1_unverified(&bytes)
            .expect("golden node must decode");
        assert_eq!(decoded.encode_v1(), bytes);
        assert_eq!(decoded.leaves.len(), 2);

        let verified = PersistedInternalNode::<StateHamtKey, String>::decode_v1_verified(
            &bytes,
            GOLDEN_ROOM_ID.as_bytes(),
            handle.structural_hash,
        )
        .expect("golden node must verify under its structural hash");
        assert_eq!(verified.structural_hash, handle.structural_hash);
        assert_eq!(verified.leaves, root.leaves);
        assert_eq!(verified.datamap, root.datamap);
        assert_eq!(verified.nodemap, root.nodemap);
    }

    #[test]
    fn golden_mthr_record_round_trips() {
        let (handle, _root, lattice) = golden_tree();
        let record = encode_state_hamt_root(
            GOLDEN_ROOM_ID.as_bytes(),
            GOLDEN_ROOM_ID,
            &handle.structural_hash,
            &lattice,
        )
        .expect("golden record must encode");

        let decoded = decode_state_hamt_root(&record).expect("golden record must decode");
        assert_eq!(decoded.prefix, GOLDEN_ROOM_ID.as_bytes());
        assert_eq!(decoded.room_id, GOLDEN_ROOM_ID);
        assert_eq!(decoded.root_hash, handle.structural_hash);
        assert_eq!(decoded.state_group_id, handle.state_group_id);
        assert_eq!(decoded.lattice, lattice);

        let reencoded = encode_state_hamt_root(
            &decoded.prefix,
            &decoded.room_id,
            &decoded.root_hash,
            &decoded.lattice,
        )
        .expect("decoded record must re-encode");
        assert_eq!(reencoded, record);
    }

    #[test]
    fn golden_format_version_constants() {
        assert_eq!(HAMT_NODE_MAGIC, b"MTHN");
        assert_eq!(HAMT_ROOT_MAGIC, b"MTHR");
        assert_eq!(HAMT_WIRE_VERSION, 0x01);
        assert_eq!(STATE_HAMT_ROOT_VERSION, 0x01);
        assert_eq!(HAMT_CODEC_VERSION, 1);
        assert_eq!(HAMT_ROUTING_VERSION, 1);
    }
}
