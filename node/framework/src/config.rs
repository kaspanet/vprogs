use vprogs_node_l1_bridge::L1BridgeConfig;
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_types::Store;

use crate::NodeVm;

/// Configuration for a [`Node`](crate::Node).
///
/// Wraps the sub-system configs and adds node-level settings. Use the `with_*` builder methods
/// to override defaults.
#[derive(Clone, Debug)]
pub struct NodeConfig<S: Store<StateSpace = StateSpace>, V: NodeVm> {
    /// Bounded capacity of the [`NodeApi`](crate::NodeApi) request channel.
    pub api_channel_capacity: usize,
    /// Execution worker pool and VM configuration.
    pub execution_config: ExecutionConfig<V>,
    /// Storage backend and read/write configuration.
    pub storage_config: StorageConfig<S>,
    /// L1 bridge connection and sync configuration.
    pub l1_bridge_config: L1BridgeConfig,
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> Default for NodeConfig<S, V> {
    fn default() -> Self {
        Self {
            api_channel_capacity: 64,
            execution_config: ExecutionConfig::default(),
            storage_config: StorageConfig::default(),
            l1_bridge_config: L1BridgeConfig::default(),
        }
    }
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> NodeConfig<S, V> {
    pub fn with_api_channel_capacity(mut self, capacity: usize) -> Self {
        self.api_channel_capacity = capacity;
        self
    }

    pub fn with_execution_config(mut self, config: ExecutionConfig<V>) -> Self {
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
}
