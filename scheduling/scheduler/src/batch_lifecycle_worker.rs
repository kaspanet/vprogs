use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

use crossbeam_queue::SegQueue;
use tokio::{runtime::Builder, sync::Notify};
use vprogs_storage_types::Store;

use crate::{Processor, ScheduledBatch};

/// Background worker that drives each batch through its lifecycle stages.
pub(crate) struct BatchLifecycleWorker<S: Store, P: Processor<S>> {
    /// Queue of batches awaiting lifecycle processing.
    queue: Arc<SegQueue<ScheduledBatch<S, P>>>,
    /// Wakes the worker when a batch is pushed or on shutdown.
    notify: Arc<Notify>,
    /// Join handle for the worker thread.
    handle: JoinHandle<()>,
}

impl<S: Store, P: Processor<S>> BatchLifecycleWorker<S, P> {
    /// Creates the queue and spawns the lifecycle worker thread.
    pub(crate) fn new() -> Self {
        let queue = Arc::new(SegQueue::new());
        let notify = Arc::new(Notify::new());
        let handle = Self::start(queue.clone(), notify.clone());
        Self { queue, notify, handle }
    }

    /// Enqueues a batch and wakes the worker.
    pub(crate) fn push(&self, batch: ScheduledBatch<S, P>) {
        self.queue.push(batch);
        self.notify.notify_one();
    }

    /// Signals shutdown and waits for the worker thread to finish.
    pub(crate) fn shutdown(self) {
        // Drop our queue handle so the worker's loop sees strong_count == 1 and exits.
        drop(self.queue);
        self.notify.notify_one();
        self.handle.join().expect("batch lifecycle worker panicked");
    }

    /// Spawns the worker thread: drains the queue, awaiting each batch then scheduling its commit.
    fn start(queue: Arc<SegQueue<ScheduledBatch<S, P>>>, notify: Arc<Notify>) -> JoinHandle<()> {
        thread::spawn(move || {
            Builder::new_current_thread().build().expect("failed to build tokio runtime").block_on(
                async move {
                    // Run until the owner drops the queue on shutdown (strong_count hits 1).
                    while Arc::strong_count(&queue) != 1 {
                        // Drain ready batches: await execution, then submit each for commit.
                        while let Some(batch) = queue.pop() {
                            batch.wait_processed().await;
                            batch.schedule_commit();
                        }

                        // Park until a batch is pushed or shutdown notifies us.
                        if Arc::strong_count(&queue) != 1 {
                            notify.notified().await
                        }
                    }
                },
            )
        })
    }
}
