/// Hash representing an empty/deleted leaf or empty subtree at any depth.
///
/// Any subtree with no live occupants hashes to this value. This avoids hashing
/// `hash_internal(EMPTY, EMPTY)` - the constant is returned directly (empty subtree compression).
pub const EMPTY_HASH: [u8; 32] = [0u8; 32];

/// Abstraction over cryptographic hash functions (Blake3, SHA-256, etc).
pub trait Hasher {
    /// Hashes arbitrary-length input to a 32-byte digest.
    fn hash(data: &[u8]) -> [u8; 32];
}
