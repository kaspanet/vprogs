use std::sync::Arc;

use tap::Tap;
use vprogs_core_types::ResourceId;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_storage_types::{ReadStore, StateSpace, WriteBatch};

#[derive(Clone, Debug)]
pub struct StateVersion {
    resource_id: ResourceId,
    version: u64,
    data: Vec<u8>,
}

impl StateVersion {
    pub fn empty(id: ResourceId) -> Self {
        Self { resource_id: id, version: 0, data: Vec::new() }
    }

    pub fn from_latest_data<S: ReadStore>(store: &S, id: ResourceId) -> Self {
        match StatePtrLatest::get(store, &id) {
            None => Self::empty(id),
            Some(version) => Self {
                resource_id: id,
                version,
                data: Self::get(store, version, &id).unwrap_or_default(),
            },
        }
    }

    /// Loads the resource's data at a specific version, or empty data if absent.
    pub fn at_version<S: ReadStore>(store: &S, version: u64, id: ResourceId) -> Self {
        Self { resource_id: id, version, data: Self::get(store, version, &id).unwrap_or_default() }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn data(&self) -> &Vec<u8> {
        &self.data
    }

    pub fn data_mut(self: &mut Arc<Self>, version: u64) -> &mut Vec<u8> {
        &mut Arc::make_mut(self).tap_mut(|s| s.version = version).data
    }

    pub fn set_data(self: &mut Arc<Self>, version: u64, data: Vec<u8>) {
        if self.data != data {
            if let Some(s) = Arc::get_mut(self) {
                s.version = version;
                s.data = data;
            } else {
                let this = Self { resource_id: self.resource_id, version, data };
                *self = Arc::new(this);
            }
        }
    }

    pub fn write_data<W: WriteBatch>(&self, wb: &mut W) {
        if !self.data.is_empty() {
            Self::put(wb, self.version, &self.resource_id, &self.data);
        }
    }

    pub fn write_latest_ptr<W: WriteBatch>(&self, wb: &mut W) {
        if !self.data.is_empty() {
            StatePtrLatest::put(wb, &self.resource_id, self.version);
        } else {
            StatePtrLatest::delete(wb, &self.resource_id);
        }
    }

    pub fn write_rollback_ptr<W: WriteBatch>(&self, wb: &mut W, batch_index: u64) {
        let version = if self.data.is_empty() { 0 } else { self.version };
        StatePtrRollback::put(wb, batch_index, &self.resource_id, version);
    }

    /// Gets the data for a specific version of a resource.
    pub fn get<S: ReadStore>(store: &S, version: u64, resource_id: &ResourceId) -> Option<Vec<u8>> {
        store.get(StateSpace::StateVersion, &Self::key(version, resource_id))
    }

    /// Stores data for a specific version of a resource.
    pub fn put<W: WriteBatch>(wb: &mut W, version: u64, resource_id: &ResourceId, data: &[u8]) {
        wb.put(StateSpace::StateVersion, &Self::key(version, resource_id), data);
    }

    /// Deletes data for a specific version of a resource.
    pub fn delete<W: WriteBatch>(wb: &mut W, version: u64, resource_id: &ResourceId) {
        wb.delete(StateSpace::StateVersion, &Self::key(version, resource_id));
    }

    /// The storage key for `(version, resource_id)`: `version (u64 BE) || resource_id (borsh)`.
    /// `ResourceId`'s borsh encoding is its 32 bytes verbatim, so the layout is fixed-width.
    pub fn key(version: u64, resource_id: &ResourceId) -> [u8; 40] {
        let id: &[u8; 32] = resource_id;
        let mut key = [0u8; 40];
        key[..8].copy_from_slice(&version.to_be_bytes());
        key[8..].copy_from_slice(id);
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key layout is load-bearing for every consumer of the version CF; pin it byte-for-byte
    /// so a change cannot slip in as a refactor.
    #[test]
    fn version_key_layout_is_version_then_resource_id() {
        let id = ResourceId::from([0xAB; 32]);
        let mut expected = [0u8; 40];
        expected[..8].copy_from_slice(&7u64.to_be_bytes());
        expected[8..].copy_from_slice(&[0xAB; 32]);
        assert_eq!(StateVersion::key(7, &id), expected);
    }
}
