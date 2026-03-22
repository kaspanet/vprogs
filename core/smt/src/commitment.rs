/// A single state change: a key and its value hash. `EMPTY_HASH` means deletion.
#[derive(Clone, Copy, Debug)]
pub struct Commitment {
    /// The 32-byte key identifying this state entry in the tree.
    pub key: [u8; 32],
    /// The 32-byte hash of the state value, or `EMPTY_HASH` for deletions.
    pub value_hash: [u8; 32],
}

impl Commitment {
    /// Creates a new commitment from a key and its value hash.
    pub fn new(key: [u8; 32], value_hash: [u8; 32]) -> Self {
        Self { key, value_hash }
    }
}
