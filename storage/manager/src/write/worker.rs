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

/// Background thread that drains the write queue and commits accumulated batches to the store.
pub struct WriteWorker<K: Store, W: WriteCmd> {
    /// Size and age thresholds that decide when a batch is flushed.
    config: WriteConfig,
    /// Store the batches are committed to.
    store: Arc<K>,
    /// Pending write commands, shared with the manager that feeds them.
    queue: CmdQueue<W>,
    /// True while the worker is parked, so a producer knows whether to wake it.
    is_parked: Arc<CachePadded<AtomicBool>>,
    /// Blocks the thread while idle; signalled to wake it.
    parker: Parker,
    /// Set on shutdown to break the run loop.
    is_shutdown: Arc<AtomicBool>,
}

impl<K: Store, W: WriteCmd> WriteWorker<K, W> {
    /// Spawns the worker thread and returns a handle to wake and join it.
    pub(crate) fn spawn(
        config: &WriteConfig,
        queue: &CmdQueue<W>,
        store: &Arc<K>,
        is_shutdown: &Arc<AtomicBool>,
    ) -> WorkerHandle {
        // Build the worker from the shared handles, with its own parker and parked flag.
        let this = Self {
            config: config.clone(),
            queue: queue.clone(),
            store: store.clone(),
            is_shutdown: is_shutdown.clone(),
            parker: Parker::new(),
            is_parked: Arc::new(CachePadded::new(AtomicBool::new(false))),
        };

        // Spawn the run loop; the returned handle shares its unparker and parked flag to wake it.
        WorkerHandle::new(
            this.parker.unparker().clone(),
            this.is_parked.clone(),
            thread::spawn(move || this.run()),
        )
    }

    /// Drains the queue into batches, flushing on a flush-now command or a full or aged batch.
    /// On shutdown, drains and commits everything still queued or accumulated: an accepted
    /// write that never commits also never opens its confirmation latch, wedging any awaiter
    /// (the proving worker parks on its receipt-write latches) and leaking the store lock.
    fn run(self) {
        // Accumulate commands into a batch and track its age for the flush backstop.
        let mut batch_cmds = Vec::with_capacity(self.config.batch_size);
        let mut write_batch = self.store.write_batch();
        let mut created = Instant::now();

        while !self.is_shutdown() {
            // Flush a full or aged batch even without an explicit flush command.
            if !batch_cmds.is_empty() && self.should_flush(batch_cmds.len(), created) {
                (write_batch, created) = self.flush(write_batch, &mut batch_cmds);
            }

            // Process the next command in order.
            match self.queue.pop() {
                (Some(cmd), _) => {
                    (write_batch, created) = self.apply(cmd, write_batch, &mut batch_cmds, created)
                }
                _ => self.park(),
            }
        }

        // Shutdown drain. Ceiling: producers are torn down before the storage manager shuts
        // down (the node stops its workers, then its managers), so draining to an empty queue
        // here cannot race a further submission; a producer still pushing after its manager
        // shut down is a teardown-ordering bug elsewhere, not a data-loss case here. Commands are
        // applied as in the loop above, so a flush-now command is committed before the next one
        // executes.
        loop {
            let mut drained_any = false;
            while let (Some(cmd), _) = self.queue.pop() {
                drained_any = true;
                (write_batch, created) = self.apply(cmd, write_batch, &mut batch_cmds, created);
            }
            if !drained_any {
                break;
            }
        }
        if !batch_cmds.is_empty() {
            self.flush(write_batch, &mut batch_cmds);
        }
    }

    /// Returns whether shutdown has been signalled.
    #[inline(always)]
    fn is_shutdown(&self) -> bool {
        self.is_shutdown.load(Ordering::Acquire)
    }

    /// Returns whether the batch is full or has outlived the flush window.
    #[inline(always)]
    fn should_flush(&self, batch_size: usize, created: Instant) -> bool {
        self.batch_is_full(batch_size) || created.elapsed() >= self.config.batch_duration
    }

    /// Returns whether the batch has reached the configured size.
    #[inline(always)]
    fn batch_is_full(&self, batch_size: usize) -> bool {
        batch_size >= self.config.batch_size
    }

    /// Executes `cmd` against the batch and flushes at once if the command asks for it. Returns the
    /// batch to continue with and the time it was started at.
    ///
    /// A command may compute what it writes from the store (a batch commit reads the tree the
    /// commit before it wrote), and the store does not show what is still in the batch. So a
    /// flush-now command is committed before the next command executes, in the shutdown drain as in
    /// the run loop.
    fn apply(
        &self,
        cmd: W,
        write_batch: K::WriteBatch,
        cmds: &mut Vec<W>,
        created: Instant,
    ) -> (K::WriteBatch, Instant) {
        let flush_now = cmd.flush_now();
        let write_batch = cmd.exec(&*self.store, write_batch);
        cmds.push(cmd);
        if flush_now { self.flush(write_batch, cmds) } else { (write_batch, created) }
    }

    /// Commits the batch, runs each command's flushed callback, and returns a fresh batch.
    fn flush(&self, write_batch: K::WriteBatch, cmds: &mut Vec<W>) -> (K::WriteBatch, Instant) {
        self.store.commit(write_batch);
        cmds.drain(..).for_each(W::flushed);
        (self.store.write_batch(), Instant::now())
    }

    /// Parks until woken or the batch window elapses, unless the queue is already non-empty.
    fn park(&self) {
        self.is_parked.store(true, Ordering::SeqCst);
        if !self.is_shutdown() && self.queue.is_empty() {
            self.parker.park_timeout(self.config.batch_duration);
        }
        self.is_parked.store(false, Ordering::SeqCst);
    }
}
