use vprogs_core_types::{BatchMetadata, Checkpoint};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::{ReadStore, WriteBatch};

/// Well-known metadata keys.
mod keys {
    /// Key for the last successfully pruned batch (index + metadata stored together).
    pub const LAST_PRUNED: &[u8] = b"last_pruned";
    /// Key for the last committed batch (index + metadata stored together).
    pub const LAST_COMMITTED: &[u8] = b"last_committed";
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
            .map(|bytes| borsh::from_slice(&bytes).expect("corrupted store: unrecoverable"))
            .unwrap_or_default()
    }

    /// Sets the last successfully pruned batch.
    pub fn set_last_pruned<M: BatchMetadata, W>(wb: &mut W, checkpoint: &Checkpoint<M>)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        wb.put(
            StateSpace::Metadata,
            keys::LAST_PRUNED,
            &borsh::to_vec(checkpoint).expect("failed to serialize Checkpoint"),
        );
    }

    /// Returns the last committed batch, or defaults if no batches have been committed yet.
    pub fn last_committed<M: BatchMetadata, S>(store: &S) -> Checkpoint<M>
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_COMMITTED)
            .map(|bytes| borsh::from_slice(&bytes).expect("corrupted store: unrecoverable"))
            .unwrap_or_default()
    }

    /// Sets the last committed batch.
    pub fn set_last_committed<M: BatchMetadata, W>(wb: &mut W, checkpoint: &Checkpoint<M>)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        wb.put(
            StateSpace::Metadata,
            keys::LAST_COMMITTED,
            &borsh::to_vec(checkpoint).expect("failed to serialize Checkpoint"),
        );
    }
}
