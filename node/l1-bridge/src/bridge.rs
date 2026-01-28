use std::sync::Arc;

use crossbeam_queue::SegQueue;
use tokio::sync::Notify;

use crate::{L1BridgeConfig, L1Event, state::BridgeState, worker::BridgeWorker};

/// The main L1 bridge that connects to Kaspa L1 and emits simplified events
/// suitable for scheduler integration.
pub struct L1Bridge {
    queue: Arc<SegQueue<L1Event>>,
    notify: Arc<Notify>,
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
        let queue = Arc::new(SegQueue::new());
        let notify = Arc::new(Notify::new());
        let worker = BridgeWorker::spawn(config, queue.clone(), notify.clone(), state.clone());
        Self { queue, notify, state, worker }
    }

    /// Shuts down the bridge and disconnects from the L1 node.
    pub fn shutdown(self) {
        self.worker.shutdown();
    }

    /// Returns the bridge state.
    pub fn state(&self) -> &Arc<BridgeState> {
        &self.state
    }

    /// Pops an event from the queue, if available.
    pub fn pop(&self) -> Option<L1Event> {
        self.queue.pop()
    }

    /// Waits until an event is available, then returns it.
    pub async fn wait_and_pop(&self) -> L1Event {
        loop {
            if let Some(event) = self.queue.pop() {
                return event;
            }
            self.notify.notified().await;
        }
    }

    /// Drains all available events into a vector.
    pub fn drain(&self) -> Vec<L1Event> {
        let mut events = Vec::new();
        while let Some(event) = self.queue.pop() {
            events.push(event);
        }
        events
    }
}
