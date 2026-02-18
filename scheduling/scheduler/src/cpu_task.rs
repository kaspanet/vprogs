use vprogs_scheduling_execution_workers::Task;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{RuntimeTx, VmInterface};

/// Tasks dispatched to the execution worker pool.
pub enum ManagerTask<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// Execute a transaction against its accessed resources.
    ExecuteTransaction(RuntimeTx<S, V>),
    /// Execute an arbitrary function on a worker thread.
    ExecuteFunction(Box<dyn FnOnce() + Send + Sync + 'static>),
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> Task for ManagerTask<S, V> {
    fn execute(self) {
        match self {
            ManagerTask::ExecuteTransaction(tx) => tx.execute(),
            ManagerTask::ExecuteFunction(func) => func(),
        }
    }
}
