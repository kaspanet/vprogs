use tap::Tap;
use vprogs_core_types::AccessMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{ResourceAccess, RuntimeBatchRef, RuntimeTxRef, StateDiff, vm_interface::VmInterface};

pub(crate) struct Resource<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    last_access: Option<ResourceAccess<S, V>>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> Default for Resource<S, V> {
    fn default() -> Self {
        Self { last_access: None }
    }
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> Resource<S, V> {
    pub(crate) fn access(
        &mut self,
        meta: &V::AccessMetadata,
        tx: &RuntimeTxRef<S, V>,
        batch: &RuntimeBatchRef<S, V>,
    ) -> ResourceAccess<S, V> {
        let (state_diff_ref, prev_access) = match self.last_access.take() {
            Some(prev_access) if prev_access.tx().belongs_to_batch(batch) => {
                assert!(prev_access.tx() != tx, "duplicate access to resource");
                (prev_access.state_diff(), Some(prev_access))
            }
            prev_access => (StateDiff::new(batch.clone(), meta.id()), prev_access),
        };

        ResourceAccess::new(meta.clone(), tx.clone(), state_diff_ref, prev_access)
            .tap(|this| self.last_access = Some(this.clone()))
    }

    /// Returns true if this resource can be evicted from the cache.
    ///
    /// A resource can be evicted if its last access belongs to a batch that has been committed.
    /// If the access reference has been dropped (upgrade fails), the resource can also be evicted.
    pub(crate) fn should_evict(&self) -> bool {
        match &self.last_access {
            Some(access) => access.was_committed(),
            None => true, // No access means safe to evict
        }
    }
}
