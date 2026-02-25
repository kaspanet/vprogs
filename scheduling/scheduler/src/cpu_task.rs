use vprogs_scheduling_execution_workers::Task;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{Processor, RuntimeTx};

/// Tasks dispatched to the execution worker pool.
pub enum ManagerTask<S: Store<StateSpace = StateSpace>, P: Processor> {
    /// Execute a transaction against its accessed resources.
    ExecuteTransaction(RuntimeTx<S, P>),
    /// Execute an arbitrary function on a worker thread.
    ExecuteFunction(Box<dyn FnOnce() + Send + Sync + 'static>),
}

impl<S: Store<StateSpace = StateSpace>, P: Processor> Task for ManagerTask<S, P> {
    fn execute(self) {
        match self {
            ManagerTask::ExecuteTransaction(tx) => tx.execute(),
            ManagerTask::ExecuteFunction(func) => func(),
        }
    }
}
