use super::*;
use crate::hamt::codec::PersistedInternalNode;
use crate::hamt::delta::isolate_delta;
use crate::state::LtHash;
use alloc::vec;
use std::sync::Arc;

#[test]
fn test_structural_hash_equivalence() {
    let key = b"dummy_server_key";
    let leaf1 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(1, 100)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(1, 100)], &[]),
    });
    let leaf2 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(1, 100)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(1, 100)], &[]),
    });
    let leaf3 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(2, 200)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(2, 200)], &[]),
    });

    // Identical leaves should have the same structural hash
    assert_eq!(leaf1.structural_hash, leaf2.structural_hash);
    assert_ne!(leaf1.structural_hash, leaf3.structural_hash);

    let internal1 = Arc::new(HamtNode {
        datamap: 0,
        nodemap: 1,
        leaves: vec![],
        children: vec![NodeRef::Resolved(leaf1.clone())],
        structural_hash: HamtNode::compute_structural_hash(
            key,
            0,
            1,
            &[],
            &[NodeRef::Resolved(leaf1.clone())],
        ),
    });

    let internal2 = Arc::new(HamtNode {
        datamap: 0,
        nodemap: 1,
        leaves: vec![],
        children: vec![NodeRef::Resolved(leaf2.clone())],
        structural_hash: HamtNode::compute_structural_hash(
            key,
            0,
            1,
            &[],
            &[NodeRef::Resolved(leaf2.clone())],
        ),
    });

    // Even though they are different Arc instances, their structural hashes must match
    assert_eq!(internal1.structural_hash, internal2.structural_hash);
}

#[test]
fn test_lthash_short_circuit() {
    let key = b"dummy_server_key";
    let leaf1 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(1, 100)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(1, 100)], &[]),
    });
    let leaf2 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(2, 200)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(2, 200)], &[]),
    });

    let lattice_a = LtHash::default();
    let lattice_b = LtHash::default();

    let mut resolver = |_hash: &StructuralHash| leaf1.clone();

    // Simulate identical sets
    let (added, removed) = isolate_delta(&leaf1, &lattice_a, &leaf2, &lattice_b, &mut resolver);
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

#[test]
fn test_diff_nodes_and_lazy_resolver() {
    let key = b"dummy_server_key";

    // Create a few basic nodes containing 1 leaf each.
    // They will act as children to our test roots.
    let leaf1 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(1, 100)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(1, 100)], &[]),
    });
    let leaf2 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(2, 200)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(2, 200)], &[]),
    });
    let leaf3 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(3, 300)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(3, 300)], &[]),
    });
    let leaf4 = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(4, 400)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(4, 400)], &[]),
    });

    // Node A will have:
    // Slot 0: leaf1 (Resolved)
    // Slot 1: leaf2 (Resolved)
    // Slot 2: leaf3 (Lazy - will need to be resolved)
    let child_a1 = NodeRef::Resolved(leaf1.clone());
    let child_a2 = NodeRef::Resolved(leaf2.clone());
    let child_a3_lazy = NodeRef::Lazy(leaf3.structural_hash);

    let root_a = Arc::new(HamtNode {
        datamap: 0,
        nodemap: 0b0111,
        leaves: vec![],
        children: vec![child_a1.clone(), child_a2.clone(), child_a3_lazy.clone()],
        structural_hash: HamtNode::compute_structural_hash(
            key,
            0,
            0b0111,
            &[],
            &[child_a1.clone(), child_a2.clone(), child_a3_lazy.clone()],
        ),
    });

    // Node B will have:
    // Slot 0: leaf1 (Resolved - matches Node A)
    // Slot 1: leaf4 (Resolved - replaces leaf2)
    // Slot 3: leaf3 (Resolved - added new slot)
    let child_b1 = NodeRef::Resolved(leaf1.clone());
    let child_b2 = NodeRef::Resolved(leaf4.clone());
    let child_b4 = NodeRef::Resolved(leaf3.clone());

    let root_b = Arc::new(HamtNode {
        datamap: 0,
        nodemap: 0b1011,
        leaves: vec![],
        children: vec![child_b1.clone(), child_b2.clone(), child_b4.clone()],
        structural_hash: HamtNode::compute_structural_hash(
            key,
            0,
            0b1011,
            &[],
            &[child_b1.clone(), child_b2.clone(), child_b4.clone()],
        ),
    });

    // Ensure lattice short-circuit does not fire
    let lattice_a = LtHash::default();
    let lattice_b = LtHash([1u16; 1024]);

    let mut resolve_called = false;
    let mut resolver = |hash: &StructuralHash| {
        if hash == &leaf3.structural_hash {
            resolve_called = true;
            leaf3.clone()
        } else {
            panic!("Unexpected lazy resolution");
        }
    };

    let (added, removed) = isolate_delta(&root_a, &lattice_a, &root_b, &lattice_b, &mut resolver);

    assert!(
        resolve_called,
        "Resolver should have been called for the lazy leaf3 child"
    );

    // Removals expected: leaf2 (slot 1 diff) and leaf3 (slot 2 removed in B)
    assert_eq!(removed.len(), 2);
    assert!(removed.contains(&(2, 200)));
    assert!(removed.contains(&(3, 300)));

    // Additions expected: leaf4 (slot 1 diff) and leaf3 (slot 3 added in B)
    assert_eq!(added.len(), 2);
    assert!(added.contains(&(4, 400)));
    assert!(added.contains(&(3, 300)));
}

#[test]
fn test_hamt_codec_types() {
    use alloc::string::String;
    use crate::hamt::codec::HamtCodec;
    let mut out = Vec::new();

    // bool
    true.encode_hamt(&mut out);
    false.encode_hamt(&mut out);
    let mut cursor = 0;
    assert_eq!(bool::decode_hamt(&out, &mut cursor), Ok(true));
    assert_eq!(bool::decode_hamt(&out, &mut cursor), Ok(false));
    assert!(bool::decode_hamt(&vec![2u8], &mut 0).is_err());

    out.clear();
    // String
    let s1 = String::from("hello");
    let s2 = String::from("");
    s1.encode_hamt(&mut out);
    s2.encode_hamt(&mut out);
    let mut cursor = 0;
    assert_eq!(String::decode_hamt(&out, &mut cursor), Ok(s1));
    assert_eq!(String::decode_hamt(&out, &mut cursor), Ok(s2));
    assert!(String::decode_hamt(&out[0..2], &mut 0).is_err());

    out.clear();
    // Vec<u8>
    let v1 = vec![1, 2, 3, 4, 5];
    let v2: Vec<u8> = vec![];
    v1.encode_hamt(&mut out);
    v2.encode_hamt(&mut out);
    let mut cursor = 0;
    assert_eq!(Vec::<u8>::decode_hamt(&out, &mut cursor), Ok(v1));
    assert_eq!(Vec::<u8>::decode_hamt(&out, &mut cursor), Ok(v2));
    assert!(Vec::<u8>::decode_hamt(&out[0..2], &mut 0).is_err());
}

#[test]
fn test_leaf_differences() {
    let key = b"dummy_server_key";

    // Node A will have leaves at slots 0, 1
    let root_a = Arc::new(HamtNode {
        datamap: 0b11,
        nodemap: 0,
        leaves: vec![(1, 100), (2, 200)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(
            key,
            0b11,
            0,
            &[(1, 100), (2, 200)],
            &[],
        ),
    });

    // Node B will have leaves at slots 1, 2
    let root_b = Arc::new(HamtNode {
        datamap: 0b110,
        nodemap: 0,
        leaves: vec![(2, 250), (3, 300)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(
            key,
            0b110,
            0,
            &[(2, 250), (3, 300)],
            &[],
        ),
    });

    let lattice_a = LtHash::default();
    let lattice_b = LtHash([1u16; 1024]);
    let mut resolver = |_hash: &StructuralHash| panic!("unexpected lazy");

    let (added, removed) = isolate_delta(&root_a, &lattice_a, &root_b, &lattice_b, &mut resolver);

    // slot 0 (true, false): removed (1, 100)
    // slot 1 (true, true): differs, removed (2, 200) added (2, 250)
    // slot 2 (false, true): added (3, 300)
    assert_eq!(removed.len(), 2);
    assert!(removed.contains(&(1, 100)));
    assert!(removed.contains(&(2, 200)));

    assert_eq!(added.len(), 2);
    assert!(added.contains(&(2, 250)));
    assert!(added.contains(&(3, 300)));
}
#[test]
fn test_collect_all_leaves_recursion() {
    let key = b"dummy_server_key";

    // Build a subtree: root -> internal -> leaf
    let leaf = Arc::new(HamtNode {
        datamap: 1,
        nodemap: 0,
        leaves: vec![(1, 100)],
        children: vec![],
        structural_hash: HamtNode::compute_structural_hash(key, 1, 0, &[(1, 100)], &[]),
    });

    let internal = Arc::new(HamtNode {
        datamap: 0,
        nodemap: 1,
        leaves: vec![],
        children: vec![NodeRef::Resolved(leaf.clone())],
        structural_hash: HamtNode::compute_structural_hash(
            key,
            0,
            1,
            &[],
            &[NodeRef::Resolved(leaf.clone())],
        ),
    });

    let root_a = Arc::new(HamtNode {
        datamap: 0,
        nodemap: 1,
        leaves: vec![],
        children: vec![NodeRef::Resolved(internal.clone())],
        structural_hash: HamtNode::compute_structural_hash(
            key,
            0,
            1,
            &[],
            &[NodeRef::Resolved(internal.clone())],
        ),
    });

    // root_b has nothing in slot 0. So root_a's slot 0 will be completely removed,
    // triggering collect_all_leaves on internal, which then recurses into its children (leaf).
    let root_b = Arc::new(HamtNode {
        datamap: 0, nodemap: 0, leaves: vec![], children: vec![],
        structural_hash: HamtNode::<i32, i32>::compute_structural_hash(key, 0, 0, &[], &[]),
    });

    let lattice_a = LtHash::default();
    let lattice_b = LtHash([1u16; 1024]);
    let mut resolver = |_hash: &StructuralHash| panic!("unexpected lazy");

    let (added, removed) = isolate_delta(&root_a, &lattice_a, &root_b, &lattice_b, &mut resolver);

    assert!(added.is_empty());
    assert_eq!(removed.len(), 1);
    assert!(removed.contains(&(1, 100)));
}

#[test]
fn test_structural_hash_builder_hasher() {
    use crate::hamt::hash::StructuralHashBuilder;
    use core::hash::Hasher;

    let builder = StructuralHashBuilder::new(b"key");
    assert_eq!(Hasher::finish(&builder), 0);
}
