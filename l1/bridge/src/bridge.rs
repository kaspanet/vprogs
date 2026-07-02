use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

use crossbeam_queue::SegQueue;
use tokio::{
    runtime::Builder,
    sync::{Notify, mpsc},
};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_types::ChainSink;
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};

use crate::{Command, L1BridgeConfig, L1Event, worker::BridgeWorker};

/// Handle to a background worker that follows the Kaspa L1 chain and drives a [`ChainSink`].
pub struct L1Bridge {
    /// Bridge events emitted by the worker.
    events: Arc<SegQueue<L1Event>>,
    /// Wakes a consumer blocked in [`wait_and_pop`] when an event is queued.
    event_signal: Arc<Notify>,
    /// Lock-free signal that stops the worker.
    shutdown: Arc<AtomicAsyncLatch>,
    /// Worker thread handle; `Option` so [`shutdown`] can join it.
    handle: Option<JoinHandle<()>>,
}

impl L1Bridge {
    /// Starts the bridge driving `sink`, applying `api_requests` against it.
    pub fn new<T>(config: L1BridgeConfig, sink: T, api_requests: mpsc::Receiver<Command<T>>) -> Self
    where
        T: ChainSink<ChainBlockMetadata, L1Transaction> + Send + 'static,
    {
        let events = Arc::new(SegQueue::new());
        let event_signal = Arc::new(Notify::new());
        let shutdown = Arc::new(AtomicAsyncLatch::new());
        let worker = BridgeWorker::spawn(
            config,
            sink,
            api_requests,
            events.clone(),
            event_signal.clone(),
            shutdown.clone(),
        );

        Self {
            events,
            event_signal,
            shutdown,
            handle: Some(thread::spawn(move || {
                // Single-threaded runtime so the worker's blocking RPC I/O never stalls the caller.
                Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime")
                    .block_on(worker);
            })),
        }
    }

    /// Non-blocking pop of the next event, if available.
    pub fn pop(&self) -> Option<L1Event> {
        self.events.pop()
    }

    /// Asynchronously waits for the next event and returns it.
    pub async fn wait_and_pop(&self) -> L1Event {
        loop {
            // Register before checking the queue so a push between `pop()` and `await` is not lost.
            let notified = self.event_signal.notified();
            if let Some(event) = self.events.pop() {
                return event;
            }
            notified.await;
        }
    }

    /// Returns all currently queued events.
    pub fn drain(&self) -> Vec<L1Event> {
        let mut events = Vec::new();
        while let Some(event) = self.events.pop() {
            events.push(event);
        }
        events
    }

    /// Signals the worker to stop and blocks until it finishes.
    pub fn shutdown(mut self) {
        self.shutdown.open();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("bridge worker panicked");
        }
    }
}

/// Signals the worker to stop if the bridge is dropped without an explicit [`shutdown`].
impl Drop for L1Bridge {
    fn drop(&mut self) {
        self.shutdown.open();
    }
}
