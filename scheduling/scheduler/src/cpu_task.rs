use vprogs_scheduling_execution_workers::Task;
use vprogs_storage_types::Store;

use crate::{Processor, ScheduledTransaction};

/// Tasks dispatched to the execution worker pool.
pub enum ManagerTask<S: Store, P: Processor<S>> {
    /// Execute a transaction against its accessed resources.
    ExecuteTransaction(ScheduledTransaction<S, P>),
    /// Execute an arbitrary function on a worker thread.
    ExecuteFunction(Box<dyn FnOnce() + Send + Sync + 'static>),
}

impl<S: Store, P: Processor<S>> Task for ManagerTask<S, P> {
    fn execute(self) {
        match self {
            ManagerTask::ExecuteTransaction(tx) => tx.execute(),
            ManagerTask::ExecuteFunction(func) => func(),
        }
    }
}
