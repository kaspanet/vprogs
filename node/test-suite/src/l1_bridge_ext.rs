use std::time::Duration;

use tokio::time::timeout;
use vprogs_node_l1_bridge::{L1Bridge, L1Event};

/// Convenience methods for [`L1Bridge`] in test scenarios.
pub trait L1BridgeExt {
    /// Collects events until one matches `predicate`, or panics on timeout.
    ///
    /// Returns all collected events up to and including the matching one.
    fn wait_for<F>(
        &self,
        timeout: Duration,
        predicate: F,
    ) -> impl std::future::Future<Output = Vec<L1Event>> + Send
    where
        F: Fn(&L1Event) -> bool + Send + Sync;
}

impl L1BridgeExt for L1Bridge {
    async fn wait_for<F>(&self, duration: Duration, predicate: F) -> Vec<L1Event>
    where
        F: Fn(&L1Event) -> bool + Send + Sync,
    {
        let fut = async {
            let mut collected = Vec::new();
            loop {
                // Drain any events that are already queued.
                while let Some(event) = self.pop() {
                    let matches = predicate(&event);
                    collected.push(event);
                    if matches {
                        return collected;
                    }
                }
                // Block until the next event arrives.
                let event = self.wait_and_pop().await;
                let matches = predicate(&event);
                collected.push(event);
                if matches {
                    return collected;
                }
            }
        };

        timeout(duration, fut).await.expect("timeout waiting for L1 bridge event")
    }
}
