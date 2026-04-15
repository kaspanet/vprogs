use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use crossbeam_utils::CachePadded;
use vprogs_storage_types::Store;

use crate::{
    ReadCmd,
    read::{ReadConfig, ReadWorker},
    utils::{CmdQueue, WorkerHandle},
};

pub struct ReadManager<K: Store, R: ReadCmd> {
    config: ReadConfig,
    queue: CmdQueue<R>,
    active_readers: Arc<CachePadded<AtomicUsize>>,
    worker_handles: Vec<WorkerHandle>,
    _marker: PhantomData<K>,
}

impl<K: Store, R: ReadCmd> ReadManager<K, R> {
    pub fn new(config: ReadConfig, store: &Arc<K>, is_shutdown: &Arc<AtomicBool>) -> Self {
        let queue = CmdQueue::new();
        let active_readers = Arc::new(CachePadded::new(AtomicUsize::new(0)));
        Self {
            worker_handles: Vec::from_iter(
                (0..config.worker_count)
                    .map(|i| ReadWorker::spawn(i, &queue, store, &active_readers, is_shutdown)),
            ),
            config,
            queue,
            active_readers,
            _marker: PhantomData,
        }
    }

    pub fn submit(&self, read: R) {
        self.tune_active_readers(self.queue.push(read) / self.config.worker_buffer_depth + 1)
    }

    pub fn shutdown(&self) {
        self.wake_readers(self.config.worker_count, true);

        for handle in &self.worker_handles {
            if let Some(handle) = handle.take_join() {
                handle.join().expect("read worker panicked")
            }
        }
    }

    fn tune_active_readers(&self, target_num: usize) {
        let observed_num = self.active_readers.load(Ordering::Relaxed);
        self.active_readers.store(target_num, Ordering::Relaxed);
        if target_num > observed_num {
            self.wake_readers(target_num, false);
        }
    }

    fn wake_readers(&self, n: usize, force: bool) {
        for worker in self.worker_handles.iter().take(n) {
            if force || worker.is_parked() {
                worker.wake();
            }
        }
    }
}
