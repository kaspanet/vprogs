use tokio::sync::mpsc::UnboundedSender;
use vprogs_l1_bridge::L1BridgeConfig;
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_types::Store;

use crate::{Processor, worker::BatchEvent};

/// Configuration for a [`Node`](crate::Node).
///
/// Wraps the sub-system configs and adds node-level settings. Use the `with_*` builder methods
/// to override defaults.
#[derive(Clone, Debug)]
pub struct NodeConfig<S: Store, P: Processor<S>> {
    /// Bounded capacity of the [`NodeApi`](crate::NodeApi) request channel.
    pub api_channel_capacity: usize,
    /// Execution worker pool and VM configuration.
    pub execution_config: ExecutionConfig<P>,
    /// Storage backend and read/write configuration.
    pub storage_config: StorageConfig<S>,
    /// L1 bridge connection and sync configuration.
    pub l1_bridge_config: L1BridgeConfig,
    /// Optional sink the worker forwards every scheduled batch (and rollback) to, so a host can
    /// drive work the framework loop itself doesn't do — chiefly settling proven bundles, whose
    /// receipts are reachable only from the live
    /// [`ScheduledBatch`](vprogs_scheduling_scheduler::ScheduledBatch) handle the worker would
    /// otherwise drop. `None` leaves the node execution-only.
    pub batch_sink: Option<UnboundedSender<BatchEvent<S, P>>>,
}

impl<S: Store, P: Processor<S>> Default for NodeConfig<S, P> {
    fn default() -> Self {
        Self {
            api_channel_capacity: 64,
            execution_config: ExecutionConfig::default(),
            storage_config: StorageConfig::default(),
            l1_bridge_config: L1BridgeConfig::default(),
            batch_sink: None,
        }
    }
}

impl<S: Store, P: Processor<S>> NodeConfig<S, P> {
    pub fn with_api_channel_capacity(mut self, capacity: usize) -> Self {
        self.api_channel_capacity = capacity;
        self
    }

    pub fn with_execution_config(mut self, config: ExecutionConfig<P>) -> Self {
        self.execution_config = config;
        self
    }

    pub fn with_storage_config(mut self, config: StorageConfig<S>) -> Self {
        self.storage_config = config;
        self
    }

    pub fn with_l1_bridge_config(mut self, config: L1BridgeConfig) -> Self {
        self.l1_bridge_config = config;
        self
    }

    /// Routes every scheduled batch (and rollback) to `sink` so a host can settle proven bundles
    /// off the batch handles the worker would otherwise drop. Leaving this unset keeps the node
    /// execution-only.
    pub fn with_batch_sink(mut self, sink: UnboundedSender<BatchEvent<S, P>>) -> Self {
        self.batch_sink = Some(sink);
        self
    }
}
