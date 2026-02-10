use vprogs_core_types::ResourceId;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::{ReadStore, WriteBatch};

/// Provides type-safe operations for the LatestPtr column family.
///
/// StatePtrLatest maps resource IDs to their current version number.
/// Key layout: `resource_id (borsh)`
/// Value layout: `version (u64 BE)`
pub struct StatePtrLatest;

impl StatePtrLatest {
    /// Gets the current version for a resource, or `None` if the resource doesn't exist.
    pub fn get<S, R>(store: &S, resource_id: &R) -> Option<u64>
    where
        S: ReadStore<StateSpace = StateSpace>,
        R: ResourceId,
    {
        let key = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        store
            .get(StateSpace::StatePtrLatest, &key)
            .map(|bytes| u64::from_be_bytes(bytes[..8].try_into().unwrap()))
    }

    /// Sets the current version for a resource.
    pub fn put<W, R>(wb: &mut W, resource_id: &R, version: u64)
    where
        W: WriteBatch<StateSpace = StateSpace>,
        R: ResourceId,
    {
        let key = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        wb.put(StateSpace::StatePtrLatest, &key, &version.to_be_bytes());
    }

    /// Deletes the latest pointer for a resource.
    pub fn delete<W, R>(wb: &mut W, resource_id: &R)
    where
        W: WriteBatch<StateSpace = StateSpace>,
        R: ResourceId,
    {
        let key = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        wb.delete(StateSpace::StatePtrLatest, &key);
    }
}
