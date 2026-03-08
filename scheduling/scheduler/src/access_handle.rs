use std::sync::Arc;

use vprogs_core_types::{AccessMetadata, AccessType};
use vprogs_state_version::StateVersion;
use vprogs_storage_types::Store;

use crate::{ResourceAccess, processor::Processor};

/// A handle to a resource's state during transaction execution.
///
/// Passed to [`Processor::process_transaction`] so the processor can read and mutate resource
/// data. On success the changes are committed; on failure they are rolled back.
pub struct AccessHandle<'a, S: Store, P: Processor> {
    state_version: Arc<StateVersion>,
    access: &'a ResourceAccess<S, P>,
}

impl<'a, S: Store, P: Processor> AccessHandle<'a, S, P> {
    /// Returns the per-batch account index for this resource.
    #[inline]
    pub fn account_index(&self) -> u32 {
        self.access.account_index()
    }

    /// Returns the access metadata for this resource.
    #[inline]
    pub fn access_metadata(&self) -> &AccessMetadata {
        self.access
    }

    /// Returns the current version number of this resource (0 if new).
    pub fn version(&self) -> u64 {
        self.state_version.version()
    }

    /// Returns the serialized resource data.
    #[inline]
    pub fn data(&self) -> &Vec<u8> {
        self.state_version.data()
    }

    /// Returns a mutable reference to the serialized resource data.
    #[inline]
    pub fn data_mut(&mut self) -> &mut Vec<u8> {
        self.state_version.data_mut()
    }

    /// Returns true if this resource was created by the current transaction (version 0).
    #[inline]
    pub fn is_new(&self) -> bool {
        self.state_version.version() == 0
    }

    pub(crate) fn new(access: &'a ResourceAccess<S, P>) -> Self {
        Self { state_version: access.read_state(), access }
    }

    pub(crate) fn commit_changes(self) {
        if self.access.access_type == AccessType::Write {
            self.access.set_written_state(self.state_version.clone());
        }
    }

    pub(crate) fn rollback_changes(self) {
        if self.access.access_type == AccessType::Write {
            self.access.set_written_state(self.access.read_state());
        }
    }
}
