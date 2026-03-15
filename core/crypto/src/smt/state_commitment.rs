/// A single state change expressed as a tree leaf update.
///
/// Implementors map domain-specific state representations (e.g. resource diffs) to the `[u8; 32]`
/// key/hash pairs that the tree operates on. The tree treats `EMPTY_HASH` as a deletion.
pub trait StateCommitment {
    /// The 32-byte key identifying this state entry in the tree.
    fn key(&self) -> [u8; 32];

    /// The 32-byte hash of the state value, or `EMPTY_HASH` for deletions.
    fn value_hash(&self) -> [u8; 32];
}

/// Convenience impl so raw `(key, value_hash)` tuples work directly as state commitments.
impl StateCommitment for ([u8; 32], [u8; 32]) {
    fn key(&self) -> [u8; 32] {
        self.0
    }

    fn value_hash(&self) -> [u8; 32] {
        self.1
    }
}

/// Convenience impl for references.
impl<T: StateCommitment> StateCommitment for &T {
    fn key(&self) -> [u8; 32] {
        (*self).key()
    }

    fn value_hash(&self) -> [u8; 32] {
        (*self).value_hash()
    }
}
