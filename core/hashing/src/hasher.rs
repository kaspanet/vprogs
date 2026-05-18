/// Abstraction over cryptographic hash functions (Blake3, SHA-256, etc).
pub trait Hasher {
    /// Hashes arbitrary-length input to a 32-byte digest.
    fn hash(data: impl AsRef<[u8]>) -> [u8; 32];

    /// Hashes a fixed-size domain tag followed by `data` to a 32-byte digest.
    ///
    /// Convenience for the single-payload domain-separated case. Defaults to delegating to
    /// [`hash_parts_with_domain`] with a one-element iterator.
    ///
    /// [`hash_parts_with_domain`]: Hasher::hash_parts_with_domain
    fn hash_with_domain<const N: usize>(domain: &[u8; N], data: impl AsRef<[u8]>) -> [u8; 32] {
        Self::hash_parts_with_domain(domain, [data])
    }

    /// Hashes a fixed-size domain tag followed by the concatenation of `parts` to a 32-byte
    /// digest.
    ///
    /// The domain tag and each part are streamed through the hasher's incremental API to avoid
    /// allocating an intermediate buffer. Use this for domain-separated hashing — e.g.
    /// distinguishing leaf hashes from internal node hashes in a Merkle tree.
    fn hash_parts_with_domain<const N: usize>(
        domain: &[u8; N],
        parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> [u8; 32];
}
