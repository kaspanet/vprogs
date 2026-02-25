use vprogs_state_space::StateSpace;
use vprogs_storage_manager::{ReadCmd, WriteCmd};
use vprogs_storage_types::{ReadStore, Store};

use crate::{
    ResourceAccess, RuntimeBatch, StateDiff, rollback::Rollback,
    transaction_processor::TransactionProcessor,
};

/// Commands dispatched to the storage manager's read worker.
pub enum Read<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> {
    /// Fetch the latest version data for a resource from disk.
    LatestData(ResourceAccess<S, V>),
}

impl<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> ReadCmd<StateSpace>
    for Read<S, V>
{
    fn exec<RS: ReadStore<StateSpace = StateSpace>>(&self, store: &RS) {
        match self {
            Read::LatestData(resource_access) => resource_access.read_latest_data(store),
        }
    }
}

/// Commands dispatched to the storage manager's write worker.
pub enum Write<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> {
    /// Persist a resource's versioned data and rollback pointer.
    StateDiff(StateDiff<S, V>),
    /// Finalize a batch by writing latest pointers and batch metadata.
    CommitBatch(RuntimeBatch<S, V>),
    /// Revert all batches after a target checkpoint.
    Rollback(Rollback<S, V>),
}

impl<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> WriteCmd<StateSpace>
    for Write<S, V>
{
    fn exec<ST: Store<StateSpace = StateSpace>>(
        &self,
        store: &ST,
        mut wb: ST::WriteBatch,
    ) -> ST::WriteBatch {
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
