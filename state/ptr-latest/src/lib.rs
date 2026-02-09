use vprogs_core_types::ResourceId;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::{ReadStore, WriteBatch};

/// Provides type-safe operations for the LatestPtr column family.
///
/// StatePtrLatest maps resource IDs to their current version number.
/// Key layout: `resource_id.to_bytes()`
/// Value layout: `version.to_be_bytes()` (u64)
pub struct StatePtrLatest;

impl StatePtrLatest {
    /// Gets the current version for a resource, or `None` if the resource doesn't exist.
    pub fn get<S, R>(store: &S, resource_id: &R) -> Option<u64>
    where
        S: ReadStore<StateSpace = StateSpace>,
        R: ResourceId,
    {
        store
            .get(StateSpace::StatePtrLatest, &resource_id.to_bytes())
            .map(|bytes| u64::from_be_bytes(bytes[..8].try_into().unwrap()))
    }

    /// Sets the current version for a resource.
    pub fn put<W, R>(wb: &mut W, resource_id: &R, version: u64)
    where
        W: WriteBatch<StateSpace = StateSpace>,
        R: ResourceId,
    {
        wb.put(StateSpace::StatePtrLatest, &resource_id.to_bytes(), &version.to_be_bytes());
    }

    /// Deletes the latest pointer for a resource.
    pub fn delete<W, R>(wb: &mut W, resource_id: &R)
    where
        W: WriteBatch<StateSpace = StateSpace>,
        R: ResourceId,
    {
        wb.delete(StateSpace::StatePtrLatest, &resource_id.to_bytes());
    }
}
