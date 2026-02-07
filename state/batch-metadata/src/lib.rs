use vprogs_state_space::StateSpace;
use vprogs_storage_manager::concat_bytes;
use vprogs_storage_types::{Store, WriteBatch};

/// Well-known field names for per-batch metadata.
mod fields {
    pub const BATCH_ID: &[u8] = b"batch_id";
}

/// Provides type-safe operations for the BatchMetadata column family.
///
/// Stores per-batch metadata keyed by `batch_index (u64 BE) || field_name`.
/// The u64 prefix enables efficient prefix iteration over all fields for a given batch.
pub struct BatchMetadata;

impl BatchMetadata {
    /// Returns the batch id (hash) for a given batch index, or a zero hash if not found.
    pub fn id<S>(store: &S, batch_index: u64) -> [u8; 32]
    where
        S: Store<StateSpace = StateSpace>,
    {
        let key = concat_bytes!(&batch_index.to_be_bytes(), fields::BATCH_ID);
        store
            .get(StateSpace::BatchMetadata, &key)
            .map(|bytes| bytes[..32].try_into().unwrap())
            .unwrap_or_default()
    }

    /// Sets the batch id (hash) for a given batch index.
    pub fn set_id<W>(store: &mut W, batch_index: u64, id: &[u8; 32])
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        let key = concat_bytes!(&batch_index.to_be_bytes(), fields::BATCH_ID);
        store.put(StateSpace::BatchMetadata, &key, id);
    }

    /// Deletes all metadata entries for a single batch index.
    pub fn delete<S>(store: &S, write_batch: &mut S::WriteBatch, batch_index: u64)
    where
        S: Store<StateSpace = StateSpace>,
    {
        for (key, _) in store.prefix_iter(StateSpace::BatchMetadata, &batch_index.to_be_bytes()) {
            write_batch.delete(StateSpace::BatchMetadata, &key);
        }
    }
}
