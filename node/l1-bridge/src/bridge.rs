use std::{sync::Arc, thread::JoinHandle};

use crossbeam_queue::SegQueue;
use tokio::{runtime::Builder, sync::Notify};

use crate::{L1BridgeConfig, L1Event, worker::BridgeWorker};

/// Bridge to the Kaspa L1 network that emits chain events.
pub struct L1Bridge {
    /// Lock-free queue for events produced by the worker.
    queue: Arc<SegQueue<L1Event>>,
    /// Signal to wake consumers waiting for events.
    event_signal: Arc<Notify>,
    /// Signal to request worker shutdown.
    shutdown: Arc<Notify>,
    /// Worker thread handle.
    handle: JoinHandle<()>,
}

impl L1Bridge {
    /// Creates and starts a new L1 bridge.
    pub fn new(config: L1BridgeConfig) -> Self {
        let queue = Arc::new(SegQueue::new());
        let event_signal = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());

        // Spawn worker in a dedicated thread with its own single-threaded tokio runtime.
        // This keeps async RPC operations isolated from the caller's runtime.
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
            // Register the notified future before checking the queue to avoid a missed-wakeup
            // race where a notification fires between pop() and notified().await.
            let notified = self.event_signal.notified();
            if let Some(event) = self.queue.pop() {
                return event;
            }
            notified.await;
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
