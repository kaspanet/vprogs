use vprogs_node_l1_bridge::{EventQueue, L1Event};

/// Extension trait for EventQueue.
pub trait EventQueueExt {
    /// Waits until an event matching the predicate is found.
    ///
    /// Returns all collected events up to and including the matching event.
    /// Use `tokio::time::timeout` to add a timeout if needed.
    fn wait_for<F>(&self, predicate: F) -> impl std::future::Future<Output = Vec<L1Event>> + Send
    where
        F: Fn(&L1Event) -> bool + Send;
}

impl EventQueueExt for EventQueue {
    async fn wait_for<F>(&self, predicate: F) -> Vec<L1Event>
    where
        F: Fn(&L1Event) -> bool + Send,
    {
        let mut collected = Vec::new();
        loop {
            // Drain any currently available events.
            while let Some(event) = self.pop() {
                let matches = predicate(&event);
                collected.push(event);
                if matches {
                    return collected;
                }
            }
            // Wait for an event and check it.
            let event = self.wait_and_pop().await;
            let matches = predicate(&event);
            collected.push(event);
            if matches {
                return collected;
            }
        }
    }
}
