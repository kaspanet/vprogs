use std::{sync::Arc, thread::JoinHandle};

use crossbeam_queue::SegQueue;
use tokio::{runtime::Builder, sync::Notify};

use crate::{L1BridgeConfig, L1Event, worker::BridgeWorker};

/// Bridge to the Kaspa L1 network that emits chain events.
///
/// Spawns a background worker thread that communicates with the L1 node over RPC. Events are
/// delivered through a lock-free queue and can be consumed via [`pop`], [`wait_and_pop`], or
/// [`drain`].
pub struct L1Bridge {
    /// Event queue shared with the worker.
    queue: Arc<SegQueue<L1Event>>,
    /// Wakes consumers blocked in [`wait_and_pop`].
    event_signal: Arc<Notify>,
    /// Signals the worker to shut down.
    shutdown: Arc<Notify>,
    /// Worker thread handle, wrapped in `Option` so [`shutdown`] can join it.
    handle: Option<JoinHandle<()>>,
}

impl L1Bridge {
    /// Creates and starts a new bridge. The worker runs on a dedicated thread with its own tokio
    /// runtime, isolated from the caller's runtime.
    pub fn new(config: L1BridgeConfig) -> Self {
        let queue = Arc::new(SegQueue::new());
        let event_signal = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());

        // Spawn the worker on a dedicated thread with its own single-threaded tokio runtime so
        // async RPC I/O doesn't block the caller's executor.
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

    /// Non-blocking pop of the next event, if available.
    pub fn pop(&self) -> Option<L1Event> {
        self.queue.pop()
    }

    /// Asynchronously waits until an event is available and returns it.
    pub async fn wait_and_pop(&self) -> L1Event {
        loop {
            // Register the notification future *before* checking the queue so that a push between
            // `pop()` and `notified().await` is not lost.
            let notified = self.event_signal.notified();
            if let Some(event) = self.queue.pop() {
                return event;
            }
            notified.await;
        }
    }

    /// Returns all currently queued events.
    pub fn drain(&self) -> Vec<L1Event> {
        let mut events = Vec::new();
        while let Some(event) = self.queue.pop() {
            events.push(event);
        }
        events
    }

    /// Signals the worker to stop and blocks until it finishes.
    pub fn shutdown(mut self) {
        self.shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("bridge worker panicked");
        }
    }
}

/// Ensures the worker thread receives a shutdown signal even if the bridge is dropped without an
/// explicit [`shutdown`] call.
impl Drop for L1Bridge {
    fn drop(&mut self) {
        self.shutdown.notify_one();
    }
}
