use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

use crossbeam_queue::SegQueue;
use tokio::{runtime::Builder, sync::Notify};
use vprogs_storage_types::Store;

use crate::{Processor, ScheduledBatch};

/// Background worker that drives each batch through its lifecycle stages.
///
/// Batches are pushed into a queue and processed sequentially: wait for all transactions to
/// execute, wait for all state diffs to persist, then submit the batch for commit and wait for
/// finalization.
pub(crate) struct BatchLifecycleWorker<S: Store, P: Processor> {
    queue: Arc<SegQueue<ScheduledBatch<S, P>>>,
    notify: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl<S: Store, P: Processor> BatchLifecycleWorker<S, P> {
    pub(crate) fn new() -> Self {
        let queue = Arc::new(SegQueue::new());
        let notify = Arc::new(Notify::new());
        let handle = Self::start(queue.clone(), notify.clone());

        Self { queue, notify, handle }
    }

    pub(crate) fn push(&self, batch: ScheduledBatch<S, P>) {
        self.queue.push(batch);
        self.notify.notify_one();
    }

    pub(crate) fn shutdown(self) {
        drop(self.queue);
        self.notify.notify_one();
        self.handle.join().expect("batch lifecycle worker panicked");
    }

    fn start(queue: Arc<SegQueue<ScheduledBatch<S, P>>>, notify: Arc<Notify>) -> JoinHandle<()> {
        thread::spawn(move || {
            Builder::new_current_thread().build().expect("failed to build tokio runtime").block_on(
                async move {
                    while Arc::strong_count(&queue) != 1 {
                        while let Some(batch) = queue.pop() {
                            batch.wait_processed().await;
                            batch.wait_persisted().await;
                            batch.schedule_commit();
                            batch.wait_committed().await;
                        }

                        if Arc::strong_count(&queue) != 1 {
                            notify.notified().await
                        }
                    }
                },
            )
        })
    }
}
