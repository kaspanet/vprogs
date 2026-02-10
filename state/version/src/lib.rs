use std::sync::Arc;

use tap::Tap;
use vprogs_core_types::ResourceId;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::concat_bytes;
use vprogs_storage_types::{ReadStore, WriteBatch};

#[derive(Debug, Eq, Hash, PartialEq)]
pub struct StateVersion<R: ResourceId> {
    resource_id: R,
    version: u64,
    data: Vec<u8>,
}

impl<R: ResourceId> StateVersion<R> {
    pub fn empty(id: R) -> Self {
        Self { resource_id: id, version: 0, data: Vec::new() }
    }

    pub fn from_latest_data<S>(store: &S, id: R) -> Self
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        match StatePtrLatest::get(store, &id) {
            None => Self::empty(id),
            Some(version) => match Self::get(store, version, &id) {
                None => panic!("missing data for resource_{:?}@v{:?}", id, version),
                Some(data) => Self { resource_id: id, version, data },
            },
        }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn data(&self) -> &Vec<u8> {
        &self.data
    }

    pub fn data_mut(self: &mut Arc<Self>) -> &mut Vec<u8> {
        &mut Arc::make_mut(self).tap_mut(|s| s.version += 1).data
    }

    pub fn write_data<W>(&self, wb: &mut W)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        Self::put(wb, self.version, &self.resource_id, &self.data);
    }

    pub fn write_latest_ptr<W>(&self, wb: &mut W)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        StatePtrLatest::put(wb, &self.resource_id, self.version);
    }

    pub fn write_rollback_ptr<W>(&self, wb: &mut W, batch_index: u64)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        StatePtrRollback::put(wb, batch_index, &self.resource_id, self.version);
    }

    /// Gets the data for a specific version of a resource.
    ///
    /// Key layout: `version (u64 BE) || resource_id (borsh)`
    pub fn get<S>(store: &S, version: u64, resource_id: &R) -> Option<Vec<u8>>
    where
        S: ReadStore<StateSpace = StateSpace>,
    {
        let rid = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        let key = concat_bytes!(&version.to_be_bytes(), &rid);
        store.get(StateSpace::StateVersion, &key)
    }

    /// Stores data for a specific version of a resource.
    ///
    /// Key layout: `version (u64 BE) || resource_id (borsh)`
    pub fn put<W>(wb: &mut W, version: u64, resource_id: &R, data: &[u8])
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        let rid = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        let key = concat_bytes!(&version.to_be_bytes(), &rid);
        wb.put(StateSpace::StateVersion, &key, data);
    }

    /// Deletes data for a specific version of a resource.
    ///
    /// Key layout: `version (u64 BE) || resource_id (borsh)`
    pub fn delete<W>(wb: &mut W, version: u64, resource_id: &R)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        let rid = borsh::to_vec(resource_id).expect("failed to serialize ResourceId");
        let key = concat_bytes!(&version.to_be_bytes(), &rid);
        wb.delete(StateSpace::StateVersion, &key);
    }
}

impl<R: ResourceId> Clone for StateVersion<R> {
    fn clone(&self) -> Self {
        Self {
            resource_id: self.resource_id.clone(),
            version: self.version,
            data: self.data.clone(),
        }
    }
}
