use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use vprogs_storage_types::Store;

use crate::{
    WriteCmd,
    utils::{CmdQueue, WorkerHandle},
    write::{WriteConfig, WriteWorker},
};

/// Queues write commands for the background worker and wakes it when needed.
pub struct WriteManager<W: WriteCmd> {
    /// Batching thresholds, also held by the worker.
    config: WriteConfig,
    /// Command queue shared with the worker.
    queue: CmdQueue<W>,
    /// Handle to wake and join the worker thread.
    worker: WorkerHandle,
    /// Set on shutdown to stop the worker.
    is_shutdown: Arc<AtomicBool>,
}

impl<W: WriteCmd> WriteManager<W> {
    /// Creates the queue, spawns the worker, and returns the manager.
    pub fn new<K: Store>(config: WriteConfig, store: &Arc<K>) -> Self {
        let queue = CmdQueue::new();
        let is_shutdown = Arc::new(AtomicBool::new(false));
        Self {
            worker: WriteWorker::spawn(&config, &queue, store, &is_shutdown),
            queue,
            config,
            is_shutdown,
        }
    }

    /// Queues a write and wakes a parked worker when it must act on it now.
    pub fn submit(&self, write: W) {
        // Push the write, capturing its flush-now flag and the resulting queue depth.
        let flush_now = write.flush_now();
        let queue_len = self.queue.push(write);

        // Wake a parked worker on a flush-now write or a full batch.
        if (flush_now || queue_len >= self.config.batch_size) && self.worker.is_parked() {
            self.worker.wake();
        }
    }

    /// Flags shutdown, wakes the worker and blocks until its thread finishes.
    pub fn shutdown(&self) {
        // Flag shutdown before waking the worker so its loop observes it and exits.
        self.is_shutdown.store(true, Ordering::Release);
        self.worker.wake();
        if let Some(handle) = self.worker.take_join() {
            handle.join().expect("write worker panicked");
        }
    }
}
