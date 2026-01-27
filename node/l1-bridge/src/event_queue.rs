use std::sync::Arc;

use crossbeam_queue::SegQueue;
use tokio::sync::Notify;

use crate::L1Event;

/// Lock-free event queue for L1 events.
#[derive(Clone)]
pub struct EventQueue {
    queue: Arc<SegQueue<L1Event>>,
    notify: Arc<Notify>,
}

impl EventQueue {
    pub(crate) fn new() -> Self {
        Self { queue: Arc::new(SegQueue::new()), notify: Arc::new(Notify::new()) }
    }

    /// Pushes an event to the queue and notifies waiters.
    pub(crate) fn push(&self, event: L1Event) {
        self.queue.push(event);
        self.notify.notify_one();
    }

    /// Pops an event from the queue, if available.
    pub fn pop(&self) -> Option<L1Event> {
        self.queue.pop()
    }

    /// Returns true if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Waits until an event is available, then returns it.
    ///
    /// This is useful for consumers that want to block until events arrive.
    pub async fn wait_and_pop(&self) -> L1Event {
        loop {
            if let Some(event) = self.queue.pop() {
                return event;
            }
            self.notify.notified().await;
        }
    }

    /// Waits for notification that events may be available.
    ///
    /// After this returns, call `pop()` to drain events.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }

    /// Drains all available events into a vector.
    pub fn drain(&self) -> Vec<L1Event> {
        let mut events = Vec::new();
        while let Some(event) = self.queue.pop() {
            events.push(event);
        }
        events
    }

    /// Waits until an event matching the predicate is found.
    ///
    /// Returns all collected events up to and including the matching event.
    /// Use `tokio::time::timeout` to add a timeout if needed.
    pub async fn wait_for<F>(&self, predicate: F) -> Vec<L1Event>
    where
        F: Fn(&L1Event) -> bool,
    {
        let mut collected = Vec::new();
        loop {
            // Drain any currently available events.
            while let Some(event) = self.queue.pop() {
                let matches = predicate(&event);
                collected.push(event);
                if matches {
                    return collected;
                }
            }
            // Wait for more events.
            self.notify.notified().await;
        }
    }
}
