use tokio::sync::{mpsc::Sender, oneshot};
use vprogs_core_types::Checkpoint;
use vprogs_node_l1_bridge::ChainBlockMetadata;
use vprogs_scheduling_scheduler::Scheduler;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{
    NodeVm,
    error::{NodeError, NodeResult},
};

/// Cloneable handle for querying the scheduler from outside the worker thread.
///
/// Sends closures over a channel to the worker thread, where they execute against the
/// [`Scheduler`]. Each closure captures a [`oneshot::Sender`] for the response. The worker
/// executes the closure and the caller awaits the oneshot.
pub struct NodeApi<S: Store<StateSpace = StateSpace>, V: NodeVm>(
    pub(crate) Sender<ApiRequest<S, V>>,
);

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> NodeApi<S, V> {
    /// Returns the last committed checkpoint.
    pub async fn last_committed(&self) -> NodeResult<Checkpoint<ChainBlockMetadata>> {
        self.with_scheduler(|s| s.batch_execution().last_committed().clone()).await
    }

    /// Returns the last processed checkpoint.
    pub async fn last_processed(&self) -> NodeResult<Checkpoint<ChainBlockMetadata>> {
        self.with_scheduler(|s| s.batch_execution().last_processed().clone()).await
    }

    /// Returns the last pruned checkpoint.
    pub async fn last_pruned(&self) -> NodeResult<Checkpoint<ChainBlockMetadata>> {
        self.with_scheduler(|s| s.pruning().last_pruned()).await
    }

    /// Returns the current pruning threshold.
    pub async fn pruning_threshold(&self) -> NodeResult<u64> {
        self.with_scheduler(|s| s.pruning().threshold()).await
    }

    /// Returns the number of resources currently cached in the scheduler.
    pub async fn cached_resource_count(&self) -> NodeResult<usize> {
        self.with_scheduler(|s| s.cached_resource_count()).await
    }

    /// Executes a closure against the scheduler on the worker thread and returns the result.
    pub async fn with_scheduler<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Scheduler<S, V>) -> R + Send + 'static,
    ) -> NodeResult<R> {
        // Create a oneshot pair - the closure will send the result back through `tx`.
        let (tx, rx) = oneshot::channel();

        // Wrap the user's closure to capture the oneshot sender. The worker will execute
        // this against &mut Scheduler and send the return value back.
        self.0
            .send(Box::new(move |scheduler| {
                // Ignore send errors - the caller may have timed out or dropped the future.
                let _ = tx.send(f(scheduler));
            }))
            .await
            .map_err(|_| NodeError::WorkerStopped)?;

        // Await the response from the worker thread.
        rx.await.map_err(|_| NodeError::RequestDropped)
    }
}

// Manual Clone impl to avoid unnecessary bounds on S and V.
impl<S: Store<StateSpace = StateSpace>, V: NodeVm> Clone for NodeApi<S, V> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// A boxed closure sent over the API channel and executed against the scheduler by the worker.
pub(crate) type ApiRequest<S, V> = Box<dyn FnOnce(&mut Scheduler<S, V>) + Send>;
