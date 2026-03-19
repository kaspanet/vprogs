/// A single state change: a key and its value hash. `EMPTY_HASH` means deletion.
#[derive(Clone, Copy)]
pub struct Commitment {
    /// The 32-byte key identifying this state entry in the tree.
    pub key: [u8; 32],
    /// The 32-byte hash of the state value, or `EMPTY_HASH` for deletions.
    pub value_hash: [u8; 32],
}

/// Identity conversion - allows `commit_diffs` to accept `&[Commitment]` directly.
impl From<&Commitment> for Commitment {
    fn from(s: &Commitment) -> Self {
        *s
    }
}
