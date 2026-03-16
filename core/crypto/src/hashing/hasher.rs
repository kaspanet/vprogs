/// Abstraction over hash functions.
///
/// Passed as a type parameter to `smt::Proof::verify()` and used by `smt::Store` so consumers can
/// swap between Blake3 (fast, host-side) and SHA-256 (risc0 precompile, guest-side).
pub trait Hasher {
    /// Hashes arbitrary-length input to a 32-byte digest.
    fn hash(data: &[u8]) -> [u8; 32];

    /// Computes the hash of an internal node with domain separation.
    ///
    /// Returns `EMPTY_HASH` without hashing when both children are empty (empty subtree
    /// compression). Prefixes with `0x00` to prevent type confusion with leaf hashes.
    fn hash_internal(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        // Empty subtree compression: avoids hashing when both children are empty.
        if *left == crate::EMPTY_HASH && *right == crate::EMPTY_HASH {
            return crate::EMPTY_HASH;
        }
        // Domain tag 0x00 distinguishes internal nodes from leaves (tag 0x01).
        let mut buf = [0u8; 65];
        buf[0] = 0x00;
        buf[1..33].copy_from_slice(left);
        buf[33..65].copy_from_slice(right);
        Self::hash(&buf)
    }

    /// Computes the hash of a shortcut leaf with domain separation.
    ///
    /// Returns `EMPTY_HASH` when `value_hash` is empty (deletion). Prefixes with `0x01` to prevent
    /// type confusion with internal node hashes.
    fn hash_leaf(key: &[u8; 32], value_hash: &[u8; 32]) -> [u8; 32] {
        // A leaf with an empty value hash represents a deletion — return EMPTY_HASH.
        if *value_hash == crate::EMPTY_HASH {
            return crate::EMPTY_HASH;
        }
        // Domain tag 0x01 distinguishes leaves from internal nodes (tag 0x00).
        let mut buf = [0u8; 65];
        buf[0] = 0x01;
        buf[1..33].copy_from_slice(key);
        buf[33..65].copy_from_slice(value_hash);
        Self::hash(&buf)
    }
}
