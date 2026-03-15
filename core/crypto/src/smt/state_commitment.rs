/// A single state change expressed as a tree leaf update: a key and its value hash.
///
/// Maps domain-specific state (e.g. resource diffs) to the `[u8; 32]` key/hash pairs that the
/// tree operates on. The tree treats `EMPTY_HASH` as a deletion.
#[derive(Clone, Copy)]
pub struct StateCommitment {
    /// The 32-byte key identifying this state entry in the tree.
    pub key: [u8; 32],
    /// The 32-byte hash of the state value, or `EMPTY_HASH` for deletions.
    pub value_hash: [u8; 32],
}
