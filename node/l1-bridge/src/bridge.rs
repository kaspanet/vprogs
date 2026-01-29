use std::{sync::Arc, thread::JoinHandle};

use crossbeam_queue::SegQueue;
use tokio::{runtime::Builder, sync::Notify};

use crate::{L1BridgeConfig, L1Event, worker::BridgeWorker};

/// The main L1 bridge that connects to Kaspa L1 and emits simplified events
/// suitable for scheduler integration.
pub struct L1Bridge {
    queue: Arc<SegQueue<L1Event>>,
    event_signal: Arc<Notify>,
    shutdown: Arc<Notify>,
    handle: Option<JoinHandle<()>>,
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
        let queue = Arc::new(SegQueue::new());
        let event_signal = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());

        let handle = std::thread::spawn({
            let queue = queue.clone();
            let event_signal = event_signal.clone();
            let shutdown = shutdown.clone();

            move || {
                let runtime = Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime");

                runtime.block_on(async {
                    if let Some(worker) = BridgeWorker::new(&config, queue, event_signal).await {
                        worker.run(shutdown).await;
                    }
                });
            }
        });

        Self { queue, event_signal, shutdown, handle: Some(handle) }
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
            self.event_signal.notified().await;
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

    /// Shuts down the bridge and disconnects from the L1 node.
    pub fn shutdown(mut self) {
        self.shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("bridge worker panicked");
        }
    }
}

impl Drop for L1Bridge {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
