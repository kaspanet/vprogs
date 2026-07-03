use tokio::sync::mpsc;
use vprogs_l1_bridge::L1Bridge;
use vprogs_scheduling_scheduler::{Scheduler, SchedulerState};
use vprogs_storage_types::Store;

use crate::{Processor, api::NodeApi, config::NodeConfig};

/// A running node that processes L1 chain blocks through the L2 scheduler.
pub struct Node<S: Store, P: Processor<S>> {
    /// Cloneable handle for interacting with the node.
    api: NodeApi<S, P>,
    /// The L1 bridge, which owns and drives the scheduler on its worker thread.
    bridge: L1Bridge,
}

impl<S: Store, P: Processor<S>> Node<S, P> {
    /// Creates and starts a new node, building the scheduler's shared state from
    /// `config.storage_config`.
    pub fn new(config: NodeConfig<S, P>) -> Self {
        let state = SchedulerState::new(config.storage_config.clone());
        Self::with_state(config, state)
    }

    /// Creates and starts a new node over a pre-built shared `state`, ignoring
    /// `config.storage_config`. Use this when the state's storage manager must be shared with a
    /// component built before the node: the processor's aggregate prover takes a
    /// [`ReceiptStore`](vprogs_scheduling_scheduler::ReceiptStore) derived from this same state, so
    /// the state must exist before that processor and hence before the node.
    pub fn with_state(config: NodeConfig<S, P>, state: SchedulerState<S, P>) -> Self {
        // Build the scheduler over the shared state - state reads the last checkpoint from store.
        let scheduler = Scheduler::with_state(config.execution_config, state);
        let (tx, rx) = mpsc::channel(config.api_channel_capacity);

        Self {
            api: NodeApi::new(scheduler.state().clone(), tx),
            bridge: L1Bridge::new(config.l1_bridge_config, scheduler, rx),
        }
    }

    /// Returns a handle for interacting with the node.
    pub fn api(&self) -> &NodeApi<S, P> {
        &self.api
    }

    /// Shuts the node down by signaling the bridge worker to shut down.
    pub fn shutdown(self) {
        self.bridge.shutdown();
    }
}
