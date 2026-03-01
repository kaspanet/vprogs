use vprogs_storage_manager::{ReadCmd, WriteCmd};
use vprogs_storage_types::{ReadStore, Store};

use crate::{ResourceAccess, ScheduledBatch, StateDiff, processor::Processor, rollback::Rollback};

/// Commands dispatched to the storage manager's read worker.
pub enum Read<S: Store, P: Processor> {
    /// Fetch the latest version data for a resource from disk.
    LatestData(ResourceAccess<S, P>),
}

impl<S: Store, P: Processor> ReadCmd for Read<S, P> {
    fn exec<RS: ReadStore>(&self, store: &RS) {
        match self {
            Read::LatestData(resource_access) => resource_access.read_latest_data(store),
        }
    }
}

/// Commands dispatched to the storage manager's write worker.
pub enum Write<S: Store, P: Processor> {
    /// Persist a resource's versioned data and rollback pointer.
    StateDiff(StateDiff<S, P>),
    /// Finalize a batch by writing latest pointers and batch metadata.
    CommitBatch(ScheduledBatch<S, P>),
    /// Revert all batches after a target checkpoint.
    Rollback(Rollback<S, P>),
}

impl<S: Store, P: Processor> WriteCmd for Write<S, P> {
    fn exec<ST: Store>(&self, store: &ST, mut wb: ST::WriteBatch) -> ST::WriteBatch {
        match self {
            Write::StateDiff(state_diff) => state_diff.write(&mut wb),
            Write::CommitBatch(batch) => batch.commit(&mut wb),
            Write::Rollback(rollback) => return rollback.execute(store, wb),
        }
        wb
    }

    fn done(self) {
        match self {
            Write::StateDiff(state_diff) => state_diff.write_done(),
            Write::CommitBatch(batch) => batch.commit_done(),
            Write::Rollback(rollback) => rollback.done(),
        }
    }
}
