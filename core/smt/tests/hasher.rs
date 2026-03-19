use vprogs_core_smt::{Blake3, EMPTY_HASH, Node};

#[test]
fn domain_separation_produces_different_hashes() {
    // Same payload but different domain tags should produce different hashes.
    let a = [1u8; 32];
    let b = [2u8; 32];
    let internal = Node::internal::<Blake3>(&a, &b);
    let leaf = Node::leaf::<Blake3>(a, b);
    assert_ne!(internal.hash(), leaf.hash());
}

#[test]
fn empty_compression_internal() {
    let node = Node::internal::<Blake3>(&EMPTY_HASH, &EMPTY_HASH);
    assert_eq!(*node.hash(), EMPTY_HASH);
}

#[test]
fn empty_compression_leaf() {
    let key = [0xABu8; 32];
    let node = Node::leaf::<Blake3>(key, EMPTY_HASH);
    assert_eq!(*node.hash(), EMPTY_HASH);
}

#[test]
fn non_empty_internal_is_not_empty() {
    let a = [1u8; 32];
    let node = Node::internal::<Blake3>(&a, &EMPTY_HASH);
    assert_ne!(*node.hash(), EMPTY_HASH);
}

#[test]
fn non_empty_leaf_is_not_empty() {
    let key = [1u8; 32];
    let value = [2u8; 32];
    let node = Node::leaf::<Blake3>(key, value);
    assert_ne!(*node.hash(), EMPTY_HASH);
}
