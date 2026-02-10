use vprogs_core_types::ResourceId;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::concat_bytes;
use vprogs_storage_types::{Store, WriteBatch};

/// Provides type-safe operations for the RollbackPtr column family.
///
/// StatePtrRollback stores the version a resource had before a batch was applied,
/// allowing state to be reverted during chain reorganization.
///
/// Key layout: `batch_index (u64 BE) || resource_id (borsh)`
/// Value layout: `old_version (u64 BE)`
pub struct StatePtrRollback;

impl StatePtrRollback {
    /// Stores the version a resource had before a batch was applied.
    pub fn put<W, R>(wb: &mut W, batch_index: u64, resource_id: &R, old_version: u64)
    where
        W: WriteBatch<StateSpace = StateSpace>,
        R: ResourceId,
    {
        let rid = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        let key = concat_bytes!(&batch_index.to_be_bytes(), &rid);
        wb.put(StateSpace::StatePtrRollback, &key, &old_version.to_be_bytes());
    }

    /// Deletes a rollback pointer entry.
    pub fn delete<W, R>(wb: &mut W, batch_index: u64, resource_id: &R)
    where
        W: WriteBatch<StateSpace = StateSpace>,
        R: ResourceId,
    {
        let rid = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        let key = concat_bytes!(&batch_index.to_be_bytes(), &rid);
        wb.delete(StateSpace::StatePtrRollback, &key);
    }

    /// Iterates all rollback pointers for a given batch index.
    ///
    /// Returns an iterator yielding `(resource_id_bytes, old_version)` pairs.
    /// The caller must decode the resource ID bytes using `borsh::from_slice`.
    pub fn iter_batch<S>(store: &S, batch_index: u64) -> impl Iterator<Item = (Vec<u8>, u64)> + '_
    where
        S: Store<StateSpace = StateSpace>,
    {
        store.prefix_iter(StateSpace::StatePtrRollback, &batch_index.to_be_bytes()).map(
            |(key, value)| {
                let resource_id_bytes = key[8..].to_vec();
                let old_version = u64::from_be_bytes(value[..8].try_into().unwrap());
                (resource_id_bytes, old_version)
            },
        )
    }
}
