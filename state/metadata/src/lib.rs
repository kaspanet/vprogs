use vprogs_state_space::StateSpace;
use vprogs_storage_types::{ReadStore, WriteBatch};

/// Well-known metadata keys.
mod keys {
    /// Key for the last successfully pruned batch index.
    pub const LAST_PRUNED_INDEX: &[u8] = b"last_pruned_index";
}

/// Provides type-safe operations for the Metadata column family.
///
/// StateMetadata stores system-level metadata such as pruning progress,
/// allowing crash-fault tolerant operations.
pub struct StateMetadata;

impl StateMetadata {
    /// Gets the last successfully pruned batch index.
    ///
    /// Returns `None` if no pruning has occurred yet.
    pub fn get_last_pruned_index<S>(store: &S) -> Option<u64>
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_PRUNED_INDEX)
            .map(|bytes| u64::from_be_bytes(bytes[..8].try_into().unwrap()))
    }

    /// Sets the last successfully pruned batch index.
    pub fn set_last_pruned_index<W>(store: &mut W, index: u64)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        store.put(StateSpace::Metadata, keys::LAST_PRUNED_INDEX, &index.to_be_bytes());
    }
}
