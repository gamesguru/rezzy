use super::*;
use crate::hamt::delta::isolate_delta;
use crate::hamt::hash::compute_structural_hash;
use crate::hamt::PersistedInternalNode;
use crate::state::LtHash;
use alloc::vec;
use std::sync::Arc;

#[test]
fn test_structural_hash_equivalence() {
    let key = b"dummy_server_key";
    let leaf1 = Arc::new(HamtNode::Leaf { key: 1, value: 100 });
    let leaf2 = Arc::new(HamtNode::Leaf { key: 1, value: 100 });
    let leaf3 = Arc::new(HamtNode::Leaf { key: 2, value: 200 });

    // Identical leaves should have the same structural hash
    assert_eq!(leaf1.structural_hash(key), leaf2.structural_hash(key));
    assert_ne!(leaf1.structural_hash(key), leaf3.structural_hash(key));

    let internal1 = Arc::new(HamtNode::Internal {
        datamap: 1,
        nodemap: 0,
        children: vec![leaf1.clone()],
        structural_hash: compute_structural_hash(key, 1, 0, &[leaf1.structural_hash(key)]),
    });

    let internal2 = Arc::new(HamtNode::Internal {
        datamap: 1,
        nodemap: 0,
        children: vec![leaf2.clone()],
        structural_hash: compute_structural_hash(key, 1, 0, &[leaf2.structural_hash(key)]),
    });

    // Even though they are different Arc instances, their structural hashes must match
    assert_eq!(
        internal1.structural_hash(key),
        internal2.structural_hash(key)
    );
}

#[test]
fn test_lthash_short_circuit() {
    let key = b"dummy_server_key";
    let leaf1 = Arc::new(HamtNode::Leaf { key: 1, value: 100 });
    let leaf2 = Arc::new(HamtNode::Leaf { key: 2, value: 200 });

    let lattice_a = LtHash::default();
    let lattice_b = LtHash::default();

    // Simulate identical sets
    let (added, removed) = isolate_delta(key, &leaf1, &lattice_a, &leaf2, &lattice_b);
    assert!(added.is_empty());
    assert!(removed.is_empty());
}

#[test]
fn test_persisted_internal_node_round_trip() {
    let node = PersistedInternalNode {
        datamap: 0b11,
        nodemap: 0b1,
        structural_hash: [0xaa; 16],
        leaves: vec![(1_i32, 10_i32), (2_i32, 20_i32)],
        child_hashes: vec![[0x11; 16]],
    };

    let encoded = node.encode_v1();
    let decoded = PersistedInternalNode::decode_v1(&encoded).expect("round-trip must decode");

    assert_eq!(decoded, node);
}

#[test]
fn test_root_handle_uses_distinct_state_group_id() {
    let lattice = LtHash::default();
    let structural_hash = [0x42; 16];
    let handle = RootHandle::from_lthash(structural_hash, &lattice);

    assert_eq!(handle.structural_hash, structural_hash);
    assert_eq!(handle.state_group_id, state_group_id_from_lthash(&lattice));
    assert_eq!(handle.state_group_id.len(), 32);
}
