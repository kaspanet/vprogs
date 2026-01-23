use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

use crossbeam_queue::SegQueue;
use tokio::{runtime::Builder, sync::Notify};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{RuntimeBatch, VmInterface};

pub(crate) struct BatchLifecycleWorker<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    queue: Arc<SegQueue<RuntimeBatch<S, V>>>,
    notify: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> BatchLifecycleWorker<S, V> {
    pub(crate) fn new(vm: V) -> Self {
        let queue = Arc::new(SegQueue::new());
        let notify = Arc::new(Notify::new());
        let handle = Self::start(queue.clone(), notify.clone(), vm);

        Self { queue, notify, handle }
    }

    pub(crate) fn push(&self, batch: RuntimeBatch<S, V>) {
        self.queue.push(batch);
        self.notify.notify_one();
    }

    pub(crate) fn shutdown(self) {
        drop(self.queue);
        self.notify.notify_one();
        self.handle.join().expect("batch lifecycle worker panicked");
    }

    fn start(
        queue: Arc<SegQueue<RuntimeBatch<S, V>>>,
        notify: Arc<Notify>,
        vm: V,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            Builder::new_current_thread().build().expect("failed to build tokio runtime").block_on(
                async move {
                    while Arc::strong_count(&queue) != 1 {
                        while let Some(batch) = queue.pop() {
                            batch.wait_processed().await;
                            vm.notarize_batch(&batch);
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
