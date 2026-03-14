use vprogs_core_crypto::{
    Blake3Hasher, EMPTY_HASH, Hasher,
    smt::{LeafEntry, MultiProof},
};
use vprogs_zk_abi::DecodedMultiProof;

#[test]
fn single_leaf_at_root_depth() {
    let key = [0xABu8; 32];
    let value_hash = *blake3::hash(b"hello").as_bytes();
    let leaf_hash = Blake3Hasher::hash_leaf(&key, &value_hash);

    // A single leaf at depth 0 means the leaf IS the root. No siblings, no topology.
    let proof = MultiProof {
        leaves: vec![LeafEntry { depth: 0, key, value_hash }],
        siblings: vec![],
        topology: vec![],
    };
    let encoded = proof.encode();
    let decoded = DecodedMultiProof::<Blake3Hasher>::decode(&encoded);

    assert_eq!(decoded.n_leaves(), 1);
    assert_eq!(decoded.leaf_depth(0), 0);
    assert!(decoded.verify(leaf_hash));
}

/// Two leaves that diverge at bit 0 — one sibling-free split at root.
#[test]
fn two_leaves_diverging_at_root() {
    let k1 = [0u8; 32]; // bit 0 = 0
    let mut k2 = [0u8; 32];
    k2[0] = 0x80; // bit 0 = 1

    let vh1 = *blake3::hash(b"left").as_bytes();
    let vh2 = *blake3::hash(b"right").as_bytes();

    let lh1 = Blake3Hasher::hash_leaf(&k1, &vh1);
    let lh2 = Blake3Hasher::hash_leaf(&k2, &vh2);
    let root = Blake3Hasher::hash_internal(&lh1, &lh2);

    // Both leaves at depth 1. Topology: one split at root (bit = 1).
    let proof = MultiProof {
        leaves: vec![
            LeafEntry { depth: 1, key: k1, value_hash: vh1 },
            LeafEntry { depth: 1, key: k2, value_hash: vh2 },
        ],
        siblings: vec![],
        topology: vec![0x01u8],
    };
    let encoded = proof.encode();
    let decoded = DecodedMultiProof::<Blake3Hasher>::decode(&encoded);

    assert_eq!(decoded.n_leaves(), 2);
    assert!(decoded.verify(root));
}

#[test]
fn compute_root_with_update() {
    let key = [0u8; 32];
    let vh_old = *blake3::hash(b"old").as_bytes();
    let vh_new = *blake3::hash(b"new").as_bytes();

    // Single leaf at depth 0.
    let proof = MultiProof {
        leaves: vec![LeafEntry { depth: 0, key, value_hash: vh_old }],
        siblings: vec![],
        topology: vec![],
    };
    let encoded = proof.encode();
    let decoded = DecodedMultiProof::<Blake3Hasher>::decode(&encoded);

    let new_root = decoded.compute_root(&[vh_new]);
    let expected = Blake3Hasher::hash_leaf(&key, &vh_new);
    assert_eq!(new_root, expected);
}

#[test]
fn empty_proof_returns_empty_hash() {
    let buf = {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_le_bytes()); // n_leaves
        b.extend_from_slice(&0u32.to_le_bytes()); // n_siblings
        b.extend_from_slice(&0u32.to_le_bytes()); // topology_len
        b
    };
    let proof = DecodedMultiProof::<Blake3Hasher>::decode(&buf);
    assert!(proof.verify(EMPTY_HASH));
}

/// Single leaf with a sibling — leaf at depth 1, sibling on the other side.
#[test]
fn single_leaf_with_sibling() {
    let key = [0u8; 32]; // bit 0 = 0, goes left
    let vh = *blake3::hash(b"data").as_bytes();
    let sibling_hash = [0xFFu8; 32];

    let lh = Blake3Hasher::hash_leaf(&key, &vh);
    let root = Blake3Hasher::hash_internal(&lh, &sibling_hash);

    // Leaf at depth 1. Topology: one bit = 0 (use sibling).
    let proof = MultiProof {
        leaves: vec![LeafEntry { depth: 1, key, value_hash: vh }],
        siblings: vec![sibling_hash],
        topology: vec![0x00u8],
    };
    let encoded = proof.encode();
    let decoded = DecodedMultiProof::<Blake3Hasher>::decode(&encoded);

    assert!(decoded.verify(root));
}
