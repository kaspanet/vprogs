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
    /// Execution worker pool and VM configuration.
    pub(crate) execution: ExecutionConfig<V>,
    /// Storage backend and read/write configuration.
    pub(crate) storage: StorageConfig<S>,
    /// L1 bridge connection and sync configuration.
    pub(crate) l1_bridge: L1BridgeConfig,
    /// Bounded capacity of the [`NodeApi`](crate::NodeApi) request channel.
    pub(crate) api_channel_capacity: usize,
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> NodeConfig<S, V> {
    /// Creates a new config with the given sub-system configs and default node-level settings.
    pub fn new(
        execution: ExecutionConfig<V>,
        storage: StorageConfig<S>,
        l1_bridge: L1BridgeConfig,
    ) -> Self {
        Self { execution, storage, l1_bridge, api_channel_capacity: 64 }
    }

    /// Sets the bounded capacity of the [`NodeApi`](crate::NodeApi) request channel.
    ///
    /// Defaults to 64.
    pub fn with_api_channel_capacity(mut self, capacity: usize) -> Self {
        self.api_channel_capacity = capacity;
        self
    }
}
