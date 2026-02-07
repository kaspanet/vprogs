use vprogs_state_space::StateSpace;
use vprogs_storage_types::{ReadStore, WriteBatch};

/// Well-known metadata keys.
mod keys {
    /// Key for the last successfully pruned batch index.
    pub const LAST_PRUNED_INDEX: &[u8] = b"last_pruned_index";
    /// Key for the tip batch index (latest committed batch).
    pub const TIP_BATCH_INDEX: &[u8] = b"tip_batch_index";
    /// Key for the tip batch id (hash of the latest committed batch).
    pub const TIP_BATCH_ID: &[u8] = b"tip_batch_id";
    /// Key for the batch id (hash) at the last pruned index.
    pub const LAST_PRUNED_BATCH_ID: &[u8] = b"last_pruned_batch_id";
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

    /// Gets the tip batch index (latest committed batch).
    pub fn get_tip_batch_index<S>(store: &S) -> Option<u64>
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::TIP_BATCH_INDEX)
            .map(|bytes| u64::from_be_bytes(bytes[..8].try_into().unwrap()))
    }

    /// Sets the tip batch index.
    pub fn set_tip_batch_index<W>(store: &mut W, index: u64)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        store.put(StateSpace::Metadata, keys::TIP_BATCH_INDEX, &index.to_be_bytes());
    }

    /// Gets the tip batch id (hash of the latest committed batch).
    pub fn get_tip_batch_id<S>(store: &S) -> Option<[u8; 32]>
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::TIP_BATCH_ID)
            .map(|bytes| bytes[..32].try_into().unwrap())
    }

    /// Sets the tip batch id.
    pub fn set_tip_batch_id<W>(store: &mut W, id: &[u8; 32])
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        store.put(StateSpace::Metadata, keys::TIP_BATCH_ID, id);
    }

    /// Deletes both tip fields (batch index and batch id).
    pub fn delete_tip<W>(store: &mut W)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        store.delete(StateSpace::Metadata, keys::TIP_BATCH_INDEX);
        store.delete(StateSpace::Metadata, keys::TIP_BATCH_ID);
    }

    /// Gets the batch id (hash) at the last pruned index.
    pub fn get_last_pruned_batch_id<S>(store: &S) -> Option<[u8; 32]>
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        store
            .get(StateSpace::Metadata, keys::LAST_PRUNED_BATCH_ID)
            .map(|bytes| bytes[..32].try_into().unwrap())
    }

    /// Sets the batch id (hash) at the last pruned index.
    pub fn set_last_pruned_batch_id<W>(store: &mut W, id: &[u8; 32])
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        store.put(StateSpace::Metadata, keys::LAST_PRUNED_BATCH_ID, id);
    }
}
