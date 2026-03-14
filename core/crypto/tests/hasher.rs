use vprogs_core_crypto::{Blake3Hasher, EMPTY_HASH, Hasher};

#[test]
fn domain_separation_produces_different_hashes() {
    // Same payload but different domain tags should produce different hashes.
    let a = [1u8; 32];
    let b = [2u8; 32];
    let internal = Blake3Hasher::hash_internal(&a, &b);
    let leaf = Blake3Hasher::hash_leaf(&a, &b);
    assert_ne!(internal, leaf);
}

#[test]
fn empty_compression_internal() {
    let result = Blake3Hasher::hash_internal(&EMPTY_HASH, &EMPTY_HASH);
    assert_eq!(result, EMPTY_HASH);
}

#[test]
fn empty_compression_leaf() {
    let key = [0xABu8; 32];
    let result = Blake3Hasher::hash_leaf(&key, &EMPTY_HASH);
    assert_eq!(result, EMPTY_HASH);
}

#[test]
fn non_empty_internal_is_not_empty() {
    let a = [1u8; 32];
    let result = Blake3Hasher::hash_internal(&a, &EMPTY_HASH);
    assert_ne!(result, EMPTY_HASH);
}

#[test]
fn non_empty_leaf_is_not_empty() {
    let key = [1u8; 32];
    let value = [2u8; 32];
    let result = Blake3Hasher::hash_leaf(&key, &value);
    assert_ne!(result, EMPTY_HASH);
}
