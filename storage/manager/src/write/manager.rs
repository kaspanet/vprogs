use std::{
    marker::PhantomData,
    sync::{Arc, atomic::AtomicBool},
};

use vprogs_storage_types::Store;

use crate::{
    WriteCmd,
    utils::{CmdQueue, WorkerHandle},
    write::{WriteConfig, WriteWorker},
};

pub struct WriteManager<K: Store, W: WriteCmd<K::StateSpace>> {
    config: WriteConfig,
    queue: CmdQueue<W>,
    worker: WorkerHandle,
    _marker: PhantomData<K>,
}

impl<K: Store, W: WriteCmd<K::StateSpace>> WriteManager<K, W> {
    pub fn new(config: WriteConfig, store: &Arc<K>, is_shutdown: &Arc<AtomicBool>) -> Self {
        let queue = CmdQueue::new();
        Self {
            worker: WriteWorker::spawn(&config, &queue, store, is_shutdown),
            queue,
            config,
            _marker: PhantomData,
        }
    }

    pub fn submit(&self, write: W) {
        if self.queue.push(write) >= self.config.batch_size && self.worker.is_parked() {
            self.worker.wake();
        }
    }

    pub fn shutdown(&self) {
        self.worker.wake();

        if let Some(handle) = self.worker.take_join() {
            handle.join().expect("write worker panicked");
        }
    }
}
