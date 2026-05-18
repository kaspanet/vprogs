/// Abstraction over cryptographic hash functions (Blake3, SHA-256, etc).
pub trait Hasher {
    /// Hashes arbitrary-length input to a 32-byte digest.
    fn hash(data: impl AsRef<[u8]>) -> [u8; 32];

    /// Hashes `data` with the given fixed-size domain to a 32-byte digest.
    ///
    /// Convenience for the single-payload domain-separated case. Defaults to delegating to
    /// [`hash_parts_with_domain`] with a one-element iterator.
    ///
    /// [`hash_parts_with_domain`]: Hasher::hash_parts_with_domain
    fn hash_with_domain<const N: usize>(domain: &[u8; N], data: impl AsRef<[u8]>) -> [u8; 32] {
        Self::hash_parts_with_domain(domain, [data])
    }

    /// Hashes the concatenation of `parts` with the given fixed-size domain to a 32-byte digest.
    ///
    /// The domain is incorporated into the hash in an implementation-specific way such that
    /// distinct domains produce distinct outputs for the same payload. Implementations stream
    /// the payload through the hasher's incremental API to avoid intermediate allocation.
    ///
    /// Use this for domain-separated hashing — e.g. distinguishing leaf hashes from internal
    /// node hashes in a Merkle tree.
    fn hash_parts_with_domain<const N: usize>(
        domain: &[u8; N],
        parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> [u8; 32];
}
