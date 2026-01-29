use std::{sync::Arc, thread::JoinHandle};

use crossbeam_queue::SegQueue;
use tokio::{runtime::Builder, sync::Notify};

use crate::{L1BridgeConfig, L1Event, worker::BridgeWorker};

/// Bridge to the Kaspa L1 network that emits chain events.
pub struct L1Bridge {
    queue: Arc<SegQueue<L1Event>>,
    event_signal: Arc<Notify>,
    shutdown: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl L1Bridge {
    /// Creates and starts a new L1 bridge.
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

        Self { queue, event_signal, shutdown, handle }
    }

    /// Pops an event from the queue.
    pub fn pop(&self) -> Option<L1Event> {
        self.queue.pop()
    }

    /// Waits for an event and returns it.
    pub async fn wait_and_pop(&self) -> L1Event {
        loop {
            if let Some(event) = self.queue.pop() {
                return event;
            }
            self.event_signal.notified().await;
        }
    }

    /// Drains all available events.
    pub fn drain(&self) -> Vec<L1Event> {
        let mut events = Vec::new();
        while let Some(event) = self.queue.pop() {
            events.push(event);
        }
        events
    }

    /// Shuts down the bridge.
    pub fn shutdown(self) {
        self.shutdown.notify_one();
        self.handle.join().expect("bridge worker panicked");
    }
}
