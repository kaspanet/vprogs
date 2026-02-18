use std::sync::Arc;

use tokio::sync::{mpsc::Sender, oneshot};
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::{Scheduler, SchedulerState};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{
    NodeVm,
    error::{NodeError, NodeResult},
};

/// Cloneable handle for interacting with the node.
///
/// Derefs to [`SchedulerState`], exposing lock-free reads for `root`, `last_committed`,
/// `last_processed`, and `storage`. For operations that need `&mut Scheduler` (e.g. pruning),
/// use [`with_scheduler`](Self::with_scheduler).
#[smart_pointer(deref(state))]
pub struct NodeApi<S: Store<StateSpace = StateSpace>, V: NodeVm> {
    /// Shared scheduler state for lock-free reads (deref target).
    state: SchedulerState<S, V>,
    /// Channel for sending closures to the worker thread for `&mut Scheduler` access.
    api_requests: Sender<ApiRequest<S, V>>,
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> NodeApi<S, V> {
    /// Creates a new API handle with shared state and a channel to the worker thread.
    pub(crate) fn new(state: SchedulerState<S, V>, sender: Sender<ApiRequest<S, V>>) -> Self {
        Self(Arc::new(NodeApiData { state, api_requests: sender }))
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
        self.api_requests
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

/// A boxed closure sent over the API channel and executed against the scheduler by the worker.
pub(crate) type ApiRequest<S, V> = Box<dyn FnOnce(&mut Scheduler<S, V>) + Send>;
