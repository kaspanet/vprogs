use vprogs_node_l1_bridge::{BlockHash, ChainBlock, L1Bridge};
use vprogs_scheduling_scheduler::Scheduler;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{NodeConfig, NodeVm, worker::NodeWorker};

/// A running node that processes L1 chain blocks through the L2 scheduler.
///
/// Created via [`Node::new`], which starts all background components (bridge, scheduler, event
/// loop). The node runs autonomously until [`shutdown`](Self::shutdown) is called.
///
/// # Startup Flow
///
/// 1. Unpack config into sub-configs
/// 2. Create [`Scheduler`] (reads `last_processed` from store as starting batch index)
/// 3. Load resume state from store (last_processed + last_pruned)
/// 4. Create [`L1Bridge`] with resume state
/// 5. Spawn [`NodeWorker`] event loop
pub struct Node<S: Store<StateSpace = StateSpace>, V: NodeVm> {
    worker: Option<NodeWorker<S, V>>,
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> Node<S, V> {
    /// Creates and starts a new node.
    pub fn new(config: NodeConfig<S, V>) -> Self {
        let (execution_config, storage_config, mut l1_bridge_config) = config.unpack();

        // Create the scheduler — internally reads last_processed as starting batch index.
        let scheduler = Scheduler::new(execution_config, storage_config);

        // Clone the VM for the worker before the scheduler takes ownership.
        let vm = scheduler.vm().clone();

        // Load resume state from the store.
        let store = scheduler.storage_manager().store().clone();

        let (tip_index, tip_id) = StateMetadata::last_processed(store.as_ref());
        let tip = ChainBlock::new(BlockHash::from_slice(&tip_id), tip_index, 0);

        let (root_index, root_id) = StateMetadata::last_pruned(store.as_ref());
        let root = ChainBlock::new(BlockHash::from_slice(&root_id), root_index, 0);

        // Configure the bridge with resume state.
        l1_bridge_config = l1_bridge_config.with_root(Some(root)).with_tip(Some(tip));
        let bridge = L1Bridge::new(l1_bridge_config);

        // Spawn the event loop.
        let worker = NodeWorker::new(scheduler, bridge, vm);

        Self { worker: Some(worker) }
    }

    /// Shuts down the node and all its components.
    ///
    /// Signals the event loop to stop, then shuts down the bridge and scheduler in order. Blocks
    /// until all background threads have joined.
    pub fn shutdown(mut self) {
        if let Some(worker) = self.worker.as_mut() {
            worker.shutdown();
        }
        self.worker = None;
    }
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> Drop for Node<S, V> {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.as_mut() {
            worker.shutdown();
        }
    }
}
