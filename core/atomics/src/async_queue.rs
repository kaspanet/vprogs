use std::sync::Arc;

use crossbeam_queue::SegQueue;
use tokio::sync::Notify;
use vprogs_core_macros::smart_pointer;

/// Lock-free, cloneable async queue backed by a [`SegQueue`] and a [`Notify`].
///
/// Producers call [`push`](Self::push) from any thread. Consumers call [`pop`](Self::pop) for
/// non-blocking access or [`wait_and_pop`](Self::wait_and_pop) for async waiting.
#[smart_pointer]
pub struct AsyncQueue<T: Send + 'static> {
    queue: SegQueue<T>,
    notify: Notify,
}

impl<T: Send + 'static> Default for AsyncQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static> AsyncQueue<T> {
    pub fn new() -> Self {
        Self(Arc::new(AsyncQueueData { queue: SegQueue::new(), notify: Notify::new() }))
    }

    /// Non-blocking pop, returns `None` if empty.
    pub fn pop(&self) -> Option<T> {
        self.queue.pop()
    }

    /// Asynchronously waits until an item is available and returns it.
    pub async fn wait_and_pop(&self) -> T {
        loop {
            let notified = self.notify.notified();
            if let Some(item) = self.queue.pop() {
                return item;
            }
            notified.await;
        }
    }

    /// Pushes an item and wakes one consumer.
    pub fn push(&self, item: T) {
        self.queue.push(item);
        self.notify.notify_one();
    }

    /// Returns a future that resolves when the queue is notified (an item was pushed).
    pub async fn notified(&self) {
        self.notify.notified().await
    }

    /// Returns true if this is the only remaining reference to the queue.
    pub fn is_singleton(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }
}
