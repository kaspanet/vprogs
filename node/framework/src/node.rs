use tokio::sync::mpsc;
use vprogs_l1_bridge::L1Bridge;
use vprogs_scheduling_scheduler::Scheduler;
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
    /// Creates and starts a new node.
    pub fn new(config: NodeConfig<S, P>) -> Self {
        // Create the scheduler and the channels for communicating via the API.
        let scheduler = Scheduler::new(config.execution_config, config.storage_config);
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
