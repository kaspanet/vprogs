use vprogs_state_space::StateSpace;
use vprogs_storage_manager::{ReadCmd, WriteCmd};
use vprogs_storage_types::{ReadStore, Store};

use crate::{
    ResourceAccess, RuntimeBatch, StateDiff, rollback::Rollback, vm_interface::VmInterface,
};

pub enum Read<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    LatestData(ResourceAccess<S, V>),
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> ReadCmd<StateSpace> for Read<S, V> {
    fn exec<RS: ReadStore<StateSpace = StateSpace>>(&self, store: &RS) {
        match self {
            Read::LatestData(resource_access) => resource_access.read_latest_data(store),
        }
    }
}

pub enum Write<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    StateDiff(StateDiff<S, V>),
    CommitBatch(RuntimeBatch<S, V>),
    Rollback(Rollback<S, V>),
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> WriteCmd<StateSpace> for Write<S, V> {
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
