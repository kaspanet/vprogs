use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crossbeam_queue::SegQueue;
use crossbeam_utils::CachePadded;
use vprogs_core_macros::smart_pointer;

/// A lock-free queue that tracks an approximate length.
#[smart_pointer]
pub struct CmdQueue<T> {
    /// The underlying lock-free queue.
    queue: SegQueue<T>,
    /// Upper-bound length: bumped before push, decremented after pop.
    approx_len: CachePadded<AtomicUsize>,
}

impl<T> CmdQueue<T> {
    /// Creates an empty queue.
    pub fn new() -> Self {
        Self(Arc::new(CmdQueueData {
            queue: SegQueue::new(),
            approx_len: CachePadded::new(AtomicUsize::new(0)),
        }))
    }

    /// Pushes a job and returns the new approximate length.
    pub fn push(&self, job: T) -> usize {
        // Bump len before pushing so that pop doesn't underflow.
        let approx_new_len = self.approx_len.fetch_add(1, Ordering::SeqCst) + 1;
        self.queue.push(job);
        approx_new_len
    }

    /// Pops a job if available, returning it with the resulting approximate length.
    pub fn pop(&self) -> (Option<T>, usize) {
        match self.queue.pop() {
            None => (None, self.approx_len()),
            element => (element, self.approx_len.fetch_sub(1, Ordering::Relaxed) - 1),
        }
    }

    /// Returns the approximate number of queued jobs.
    #[inline(always)]
    pub fn approx_len(&self) -> usize {
        self.approx_len.load(Ordering::Relaxed)
    }

    /// Returns true if the queue is empty.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.approx_len.load(Ordering::SeqCst) == 0
    }
}
