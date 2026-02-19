use std::{
    sync::Arc,
    thread::{JoinHandle, spawn},
};

use tokio::{runtime::Builder, sync::mpsc};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_node_l1_bridge::L1Bridge;
use vprogs_scheduling_scheduler::Scheduler;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{NodeVm, api::NodeApi, config::NodeConfig, worker::NodeWorker};

/// A running node that processes L1 chain blocks through the L2 scheduler.
///
/// Created via [`Node::new`], which starts all background components (bridge, scheduler, event
/// loop). The node runs autonomously until [`shutdown`](Self::shutdown) is called.
pub struct Node<S: Store<StateSpace = StateSpace>, V: NodeVm> {
    /// Cloneable handle for interacting with the node.
    api: NodeApi<S, V>,
    /// Worker thread running the event loop.
    handle: Option<JoinHandle<()>>,
    /// Signal to stop the event loop.
    shutdown: Arc<AtomicAsyncLatch>,
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> Node<S, V> {
    /// Creates and starts a new node.
    pub fn new(config: NodeConfig<S, V>) -> Self {
        // Create the scheduler - internally reads last checkpoint from store.
        let scheduler = Scheduler::new(config.execution_config, config.storage_config);

        // Configure the bridge with resume state. Root is the oldest surviving batch
        // (initialized on first commit, advanced on prune). On a fresh database both
        // root and tip are default (index 0), so the bridge starts from genesis.
        let root = (*scheduler.state().root()).clone();
        let tip = (*scheduler.state().last_committed()).clone();
        let bridge =
            L1Bridge::new(config.l1_bridge_config.with_root(Some(root)).with_tip(Some(tip)));

        // Create the shutdown latch and the API.
        let shutdown = Arc::new(AtomicAsyncLatch::new());
        let (tx, rx) = mpsc::channel(config.api_channel_capacity);
        let api = NodeApi::new(scheduler.state().clone(), tx);

        // Spawn the worker on a dedicated thread.
        let worker = NodeWorker::spawn(bridge, scheduler, rx, shutdown.clone());
        Self {
            handle: Some(spawn(move || {
                Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime")
                    .block_on(worker)
            })),
            shutdown,
            api,
        }
    }

    /// Returns a handle for interacting with the node.
    pub fn api(&self) -> &NodeApi<S, V> {
        &self.api
    }

    /// Shuts down the node and all its components.
    ///
    /// Signals the event loop to stop, then waits for the worker thread to finish. The worker
    /// shuts down the bridge and scheduler in order before exiting.
    pub fn shutdown(mut self) {
        self.shutdown.open();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("node worker panicked");
        }
    }
}

/// Ensures the worker thread receives a shutdown signal even if the node is dropped without an
/// explicit [`shutdown`] call.
impl<S: Store<StateSpace = StateSpace>, V: NodeVm> Drop for Node<S, V> {
    fn drop(&mut self) {
        self.shutdown.open();
    }
}
