use std::sync::Arc;

use crate::{EventQueue, L1BridgeConfig, state::BridgeState, worker::BridgeWorker};

/// The main L1 bridge that connects to Kaspa L1 and emits simplified events
/// suitable for scheduler integration.
pub struct L1Bridge {
    event_queue: EventQueue,
    state: Arc<BridgeState>,
    worker: BridgeWorker,
}

impl L1Bridge {
    /// Creates and starts a new L1 bridge with the given configuration.
    ///
    /// The bridge immediately spawns a background worker that:
    /// 1. Connects to the L1 node
    /// 2. Performs initial sync from last_processed to current tip
    /// 3. Emits BlockAdded events in order with sequential indices
    /// 4. Emits Synced event when caught up
    /// 5. Switches to live streaming mode
    pub fn new(config: L1BridgeConfig) -> Self {
        let state = Arc::new(BridgeState::new(config.last_processed, config.last_finalized));
        let event_queue = EventQueue::new();
        let worker = BridgeWorker::spawn(config, event_queue.clone(), state.clone());
        Self { event_queue, state, worker }
    }

    /// Shuts down the bridge and disconnects from the L1 node.
    pub fn shutdown(self) {
        self.worker.shutdown();
    }

    /// Returns the event queue for consuming L1 events.
    pub fn event_queue(&self) -> &EventQueue {
        &self.event_queue
    }

    /// Returns the bridge state.
    pub fn state(&self) -> &Arc<BridgeState> {
        &self.state
    }
}
