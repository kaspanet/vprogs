use vprogs_core_types::BatchMetadata;
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
    /// Returns the last successfully pruned batch (index and metadata), or defaults if no
    /// pruning has occurred yet.
    pub fn last_pruned<M: BatchMetadata, S>(store: &S) -> (u64, M)
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_PRUNED)
            .map(|bytes| {
                let index = u64::from_be_bytes(bytes[..8].try_into().unwrap());
                let metadata = M::from_bytes(&bytes[8..]);
                (index, metadata)
            })
            .unwrap_or_default()
    }

    /// Sets the last successfully pruned batch (index and metadata).
    pub fn set_last_pruned<M: BatchMetadata, W>(store: &mut W, index: u64, metadata: &M)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        let metadata_bytes = metadata.to_bytes();
        let mut value = Vec::with_capacity(8 + metadata_bytes.len());
        value.extend_from_slice(&index.to_be_bytes());
        value.extend_from_slice(&metadata_bytes);
        store.put(StateSpace::Metadata, keys::LAST_PRUNED, &value);
    }

    /// Returns the last processed batch (index and metadata), or defaults if no batches have
    /// been processed yet.
    pub fn last_processed<M: BatchMetadata, S>(store: &S) -> (u64, M)
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_PROCESSED)
            .map(|bytes| {
                let index = u64::from_be_bytes(bytes[..8].try_into().unwrap());
                let metadata = M::from_bytes(&bytes[8..]);
                (index, metadata)
            })
            .unwrap_or_default()
    }

    /// Sets the last processed batch (index and metadata).
    pub fn set_last_processed<M: BatchMetadata, W>(store: &mut W, index: u64, metadata: &M)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        let metadata_bytes = metadata.to_bytes();
        let mut value = Vec::with_capacity(8 + metadata_bytes.len());
        value.extend_from_slice(&index.to_be_bytes());
        value.extend_from_slice(&metadata_bytes);
        store.put(StateSpace::Metadata, keys::LAST_PROCESSED, &value);
    }
}
