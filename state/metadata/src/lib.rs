use vprogs_core_types::{BatchMetadata, Checkpoint};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::{ReadStore, WriteBatch};

/// Well-known metadata keys.
mod keys {
    /// Key for the last successfully pruned batch (index + metadata stored together).
    pub const LAST_PRUNED: &[u8] = b"last_pruned";
    /// Key for the last processed batch (index + metadata stored together).
    pub const LAST_PROCESSED: &[u8] = b"last_processed";
}

/// Provides type-safe operations for the Metadata column family.
///
/// StateMetadata stores system-level metadata such as pruning progress,
/// allowing crash-fault tolerant operations.
pub struct StateMetadata;

impl StateMetadata {
    /// Returns the last successfully pruned batch, or defaults if no pruning has occurred yet.
    pub fn last_pruned<M: BatchMetadata, S>(store: &S) -> Checkpoint<M>
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_PRUNED)
            .map(|bytes| Checkpoint::from_bytes(&bytes))
            .unwrap_or_default()
    }

    /// Sets the last successfully pruned batch.
    pub fn set_last_pruned<M: BatchMetadata, W>(store: &mut W, checkpoint: &Checkpoint<M>)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        store.put(StateSpace::Metadata, keys::LAST_PRUNED, &checkpoint.to_bytes());
    }

    /// Returns the last processed batch, or defaults if no batches have been processed yet.
    pub fn last_processed<M: BatchMetadata, S>(store: &S) -> Checkpoint<M>
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_PROCESSED)
            .map(|bytes| Checkpoint::from_bytes(&bytes))
            .unwrap_or_default()
    }

    /// Sets the last processed batch.
    pub fn set_last_processed<M: BatchMetadata, W>(store: &mut W, checkpoint: &Checkpoint<M>)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        store.put(StateSpace::Metadata, keys::LAST_PROCESSED, &checkpoint.to_bytes());
    }
}
