use vprogs_core_types::ResourceId;

/// A single state change: a resource ID and its value hash. `EMPTY_HASH` means deletion.
#[derive(Clone, Copy, Debug)]
pub struct Commitment {
    /// The resource this commitment applies to.
    pub key: ResourceId,
    /// The 32-byte hash of the state value, or `EMPTY_HASH` for deletions.
    pub value_hash: [u8; 32],
}

impl Commitment {
    /// Creates a new commitment from a resource ID and its value hash.
    pub fn new(key: ResourceId, value_hash: [u8; 32]) -> Self {
        Self { key, value_hash }
    }
}
