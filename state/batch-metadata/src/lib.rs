use vprogs_core_types::BatchMetadata as BatchMetadataTrait;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::{Store, WriteBatch};

/// Provides type-safe operations for the BatchMetadata column family.
///
/// Stores a single serialized metadata blob per batch, keyed by `batch_index (u64 BE)`.
pub struct BatchMetadata;

impl BatchMetadata {
    /// Returns the deserialized metadata for a batch, or `M::default()` if not found.
    pub fn get<M: BatchMetadataTrait, S>(store: &S, batch_index: u64) -> M
    where
        S: Store<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::BatchMetadata, &batch_index.to_be_bytes())
            .map(|bytes| M::from_bytes(&bytes))
            .unwrap_or_default()
    }

    /// Stores serialized metadata for a batch.
    pub fn set<M: BatchMetadataTrait, W>(store: &mut W, batch_index: u64, metadata: &M)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        store.put(StateSpace::BatchMetadata, &batch_index.to_be_bytes(), &metadata.to_bytes());
    }

    /// Deletes the metadata entry for a single batch index.
    pub fn delete<W>(write_batch: &mut W, batch_index: u64)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        write_batch.delete(StateSpace::BatchMetadata, &batch_index.to_be_bytes());
    }
}
