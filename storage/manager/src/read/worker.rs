use std::{
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use crossbeam_utils::{CachePadded, sync::Parker};
use vprogs_storage_types::Store;

use crate::{
    ReadCmd,
    utils::{CmdQueue, WorkerHandle},
};

pub struct ReadWorker<K: Store, R: ReadCmd> {
    id: usize,
    queue: CmdQueue<R>,
    store: Arc<K>,
    active_readers: Arc<CachePadded<AtomicUsize>>,
    is_shutdown: Arc<AtomicBool>,
    parker: Parker,
    is_parked: Arc<CachePadded<AtomicBool>>,
}

impl<K: Store, R: ReadCmd> ReadWorker<K, R> {
    pub(crate) fn spawn(
        id: usize,
        queue: &CmdQueue<R>,
        store: &Arc<K>,
        active_readers: &Arc<CachePadded<AtomicUsize>>,
        is_shutdown: &Arc<AtomicBool>,
    ) -> WorkerHandle {
        let this = Self {
            id,
            queue: queue.clone(),
            store: store.clone(),
            active_readers: active_readers.clone(),
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
        while !self.is_shutdown() {
            match self.queue.pop() {
                (Some(cmd), _) => {
                    cmd.exec(self.store.deref());

                    if self.should_park() {
                        self.park()
                    }
                }
                _ => self.park(),
            }
        }
    }

    #[inline(always)]
    fn is_shutdown(&self) -> bool {
        self.is_shutdown.load(Ordering::Acquire)
    }

    #[inline(always)]
    fn should_park(&self) -> bool {
        self.id >= self.active_readers.load(Ordering::Relaxed)
    }

    fn park(&self) {
        self.is_parked.store(true, Ordering::Relaxed);
        if !self.is_shutdown() && self.should_park() {
            self.parker.park();
        }
        self.is_parked.store(false, Ordering::Relaxed);
    }
}
