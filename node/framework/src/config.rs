use vprogs_node_l1_bridge::L1BridgeConfig;
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_types::Store;

use crate::NodeVm;

/// Configuration for a [`Node`](crate::Node).
///
/// Combines the execution, storage, and L1 bridge sub-configs into a single builder.
///
/// # Example
///
/// ```ignore
/// let config = NodeConfig::default()
///     .with_execution_config(execution_config)
///     .with_storage_config(storage_config)
///     .with_l1_bridge_config(l1_bridge_config);
/// ```
pub struct NodeConfig<S: Store<StateSpace = StateSpace>, V: NodeVm> {
    pub(crate) execution_config: ExecutionConfig<V>,
    pub(crate) storage_config: StorageConfig<S>,
    pub(crate) l1_bridge_config: L1BridgeConfig,
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> NodeConfig<S, V> {
    /// Creates a new config with default sub-configs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the execution configuration (worker count, VM instance).
    pub fn with_execution_config(mut self, config: ExecutionConfig<V>) -> Self {
        self.execution_config = config;
        self
    }

    /// Sets the storage configuration (store, read/write tuning).
    pub fn with_storage_config(mut self, config: StorageConfig<S>) -> Self {
        self.storage_config = config;
        self
    }

    /// Sets the L1 bridge configuration (network, URL, resume state).
    pub fn with_l1_bridge_config(mut self, config: L1BridgeConfig) -> Self {
        self.l1_bridge_config = config;
        self
    }

    /// Unpacks the config into its constituent sub-configs.
    pub(crate) fn unpack(self) -> (ExecutionConfig<V>, StorageConfig<S>, L1BridgeConfig) {
        (self.execution_config, self.storage_config, self.l1_bridge_config)
    }
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> Default for NodeConfig<S, V> {
    fn default() -> Self {
        Self {
            execution_config: ExecutionConfig::default(),
            storage_config: StorageConfig::default(),
            l1_bridge_config: L1BridgeConfig::default(),
        }
    }
}
