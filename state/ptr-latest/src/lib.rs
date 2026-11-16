use vprogs_core_types::ResourceId;
use vprogs_storage_types::{ReadStore, StateSpace, Store, WriteBatch};

/// Provides type-safe operations for the LatestPtr column family.
///
/// StatePtrLatest maps resource IDs to their current version number.
///
/// Key layout: `resource_id (borsh)`
/// Value layout: `version (u64 BE)`
pub struct StatePtrLatest;

impl StatePtrLatest {
    /// Gets the current version for a resource, or `None` if the resource doesn't exist.
    pub fn get<S>(store: &S, resource_id: &ResourceId) -> Option<u64>
    where
        S: ReadStore,
    {
        let key = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        store
            .get(StateSpace::StatePtrLatest, &key)
            .map(|bytes| u64::from_be_bytes(bytes[..8].try_into().unwrap()))
    }

    /// Sets the current version for a resource.
    pub fn put<W>(wb: &mut W, resource_id: &ResourceId, version: u64)
    where
        W: WriteBatch,
    {
        let key = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        wb.put(StateSpace::StatePtrLatest, &key, &version.to_be_bytes());
    }

    /// Deletes the latest pointer for a resource.
    pub fn delete<W>(wb: &mut W, resource_id: &ResourceId)
    where
        W: WriteBatch,
    {
        let key = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        wb.delete(StateSpace::StatePtrLatest, &key);
    }

    /// Enumerate every (resource_id, current_version) pair. Full scan of the latest-ptr CF.
    pub fn iter_all<S>(store: &S) -> impl Iterator<Item = (ResourceId, u64)> + '_
    where
        S: Store,
    {
        store.scan(StateSpace::StatePtrLatest).map(|(key, value)| {
            let resource_id: ResourceId =
                borsh::from_slice(&key).expect("corrupted latest-ptr resource id");
            let version =
                u64::from_be_bytes(value[..8].try_into().expect("corrupted latest-ptr version"));
            (resource_id, version)
        })
    }
}

#[cfg(test)]
mod tests {
    use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
    use vprogs_storage_types::Store;

    use super::*;

    #[test]
    fn iter_all_returns_every_latest_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path());

        let a = ResourceId::from([1u8; 32]);
        let b = ResourceId::from([2u8; 32]);
        let mut wb = store.write_batch();
        StatePtrLatest::put(&mut wb, &a, 7);
        StatePtrLatest::put(&mut wb, &b, 42);
        store.commit(wb);

        let mut got: Vec<(ResourceId, u64)> = StatePtrLatest::iter_all(&store).collect();
        got.sort_by_key(|(_, v)| *v);
        assert_eq!(got, vec![(a, 7), (b, 42)]);
    }
}
