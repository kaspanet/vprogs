use vprogs_state_space::StateSpace;
use vprogs_storage_types::{ReadStore, WriteBatch};

/// Well-known metadata keys.
mod keys {
    /// Key for the last successfully pruned batch (index + id stored together).
    pub const LAST_PRUNED: &[u8] = b"last_pruned";
    /// Key for the last processed batch (index + id stored together).
    pub const LAST_PROCESSED: &[u8] = b"last_processed";
}

/// Provides type-safe operations for the Metadata column family.
///
/// StateMetadata stores system-level metadata such as pruning progress,
/// allowing crash-fault tolerant operations.
pub struct StateMetadata;

impl StateMetadata {
    /// Returns the last successfully pruned batch (index and id), or `(0, [0; 32])` if no
    /// pruning has occurred yet.
    pub fn last_pruned<S>(store: &S) -> (u64, [u8; 32])
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_PRUNED)
            .map(|bytes| {
                let index = u64::from_be_bytes(bytes[..8].try_into().unwrap());
                let id: [u8; 32] = bytes[8..40].try_into().unwrap();
                (index, id)
            })
            .unwrap_or_default()
    }

    /// Sets the last successfully pruned batch (index and id).
    pub fn set_last_pruned<W>(store: &mut W, index: u64, id: &[u8; 32])
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        let mut value = [0u8; 40];
        value[..8].copy_from_slice(&index.to_be_bytes());
        value[8..40].copy_from_slice(id);
        store.put(StateSpace::Metadata, keys::LAST_PRUNED, &value);
    }

    /// Returns the last processed batch (index and id), or `(0, [0; 32])` if no batches have
    /// been processed yet.
    pub fn last_processed<S>(store: &S) -> (u64, [u8; 32])
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_PROCESSED)
            .map(|bytes| {
                let index = u64::from_be_bytes(bytes[..8].try_into().unwrap());
                let id: [u8; 32] = bytes[8..40].try_into().unwrap();
                (index, id)
            })
            .unwrap_or_default()
    }

    /// Sets the last processed batch (index and id).
    pub fn set_last_processed<W>(store: &mut W, index: u64, id: &[u8; 32])
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        let mut value = [0u8; 40];
        value[..8].copy_from_slice(&index.to_be_bytes());
        value[8..40].copy_from_slice(id);
        store.put(StateSpace::Metadata, keys::LAST_PROCESSED, &value);
    }
}
