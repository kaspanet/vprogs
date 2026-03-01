use tap::Tap;
use vprogs_core_types::AccessMetadata;
use vprogs_storage_types::Store;

use crate::{
    ResourceAccess, ScheduledBatchRef, ScheduledTransactionRef, StateDiff, processor::Processor,
};

pub(crate) struct Resource<S: Store, P: Processor> {
    last_access: Option<ResourceAccess<S, P>>,
}

impl<S: Store, P: Processor> Default for Resource<S, P> {
    fn default() -> Self {
        Self { last_access: None }
    }
}

impl<S: Store, P: Processor> Resource<S, P> {
    pub(crate) fn access(
        &mut self,
        access_metadata: &AccessMetadata,
        tx: &ScheduledTransactionRef<S, P>,
        batch: &ScheduledBatchRef<S, P>,
    ) -> ResourceAccess<S, P> {
        let (state_diff_ref, prev_access) = match self.last_access.take() {
            Some(prev_access) if prev_access.tx().belongs_to_batch(batch) => {
                assert!(prev_access.tx() != tx, "duplicate access to resource");
                (prev_access.state_diff(), Some(prev_access))
            }
            prev_access => {
                (StateDiff::new(batch.clone(), access_metadata.resource_id), prev_access)
            }
        };

        ResourceAccess::new(*access_metadata, tx.clone(), state_diff_ref, prev_access)
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
