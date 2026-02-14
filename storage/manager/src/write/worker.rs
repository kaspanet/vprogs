use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
};

use crossbeam_utils::{CachePadded, sync::Parker};
use vprogs_storage_types::Store;

use crate::{
    WriteCmd,
    utils::{CmdQueue, WorkerHandle},
    write::WriteConfig,
};

pub struct WriteWorker<K: Store, W: WriteCmd<K::StateSpace>> {
    config: WriteConfig,
    store: Arc<K>,
    queue: CmdQueue<W>,
    is_parked: Arc<CachePadded<AtomicBool>>,
    parker: Parker,
    is_shutdown: Arc<AtomicBool>,
}

impl<K: Store, W: WriteCmd<K::StateSpace>> WriteWorker<K, W> {
    pub(crate) fn spawn(
        config: &WriteConfig,
        queue: &CmdQueue<W>,
        store: &Arc<K>,
        is_shutdown: &Arc<AtomicBool>,
    ) -> WorkerHandle {
        let this = Self {
            config: config.clone(),
            queue: queue.clone(),
            store: store.clone(),
            is_shutdown: is_shutdown.clone(),
            parker: Parker::new(),
            is_parked: Arc::new(CachePadded::new(AtomicBool::new(false))),
        };

        WorkerHandle::new(
            this.parker.unparker().clone(),
            this.is_parked.clone(),
            thread::spawn(move || this.run()),
        )
    }

    fn run(self) {
        let mut batch_cmds = Vec::with_capacity(self.config.batch_size);
        let mut write_batch = self.store.write_batch();
        let mut created = Instant::now();

        while !self.is_shutdown() {
            if !batch_cmds.is_empty() && self.should_commit(batch_cmds.len(), created) {
                self.store.commit(write_batch);
                batch_cmds.drain(..).for_each(W::done);

                write_batch = self.store.write_batch();
                created = Instant::now();
            }

            match self.queue.pop() {
                (Some(cmd), _) => {
                    write_batch = cmd.exec(&*self.store, write_batch);
                    batch_cmds.push(cmd);
                }
                _ => self.park(),
            }
        }
    }

    fn should_commit(&self, batch_size: usize, created: Instant) -> bool {
        self.batch_is_full(batch_size) || created.elapsed() >= self.config.batch_duration
    }

    #[inline(always)]
    fn batch_is_full(&self, batch_size: usize) -> bool {
        batch_size >= self.config.batch_size
    }

    #[inline(always)]
    fn is_shutdown(&self) -> bool {
        self.is_shutdown.load(Ordering::Acquire)
    }

    fn park(&self) {
        self.is_parked.store(true, Ordering::Relaxed);
        if !self.is_shutdown() && self.queue.is_empty() {
            self.parker.park_timeout(self.config.batch_duration);
        }
        self.is_parked.store(false, Ordering::Relaxed);
    }
}
