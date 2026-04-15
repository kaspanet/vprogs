use std::sync::Arc;

use tokio::sync::mpsc;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_types::SchedulerTransaction;
use vprogs_l1_bridge::{L1Bridge, L1Event};
use vprogs_l1_types::L1Transaction;
use vprogs_scheduling_scheduler::Scheduler;
use vprogs_storage_types::Store;

use crate::{Processor, api::ApiRequest};

/// Owns the scheduler and bridge, processing L1 events through the L2 scheduler.
///
/// Consumes [`L1Event`]s from the bridge, extracts access metadata from L1 transaction payloads,
/// and feeds the resulting transactions to the scheduler in order.
pub(crate) struct NodeWorker<S: Store, P: Processor<S>> {
    /// L1 chain event source - the primary data entry point into the node.
    bridge: L1Bridge,
    /// Transaction scheduler and execution engine.
    scheduler: Scheduler<S, P>,
    /// Incoming [`NodeApi`](crate::NodeApi) requests.
    api_requests: mpsc::Receiver<ApiRequest<S, P>>,
    /// Signal to stop the event loop.
    shutdown: Arc<AtomicAsyncLatch>,
}

impl<S: Store, P: Processor<S>> NodeWorker<S, P> {
    /// Creates the worker and runs the event loop until shutdown is signaled or a fatal error
    /// occurs. Shuts down the bridge and scheduler before returning.
    pub(crate) async fn spawn(
        bridge: L1Bridge,
        scheduler: Scheduler<S, P>,
        api_requests: mpsc::Receiver<ApiRequest<S, P>>,
        shutdown: Arc<AtomicAsyncLatch>,
    ) {
        Self { bridge, scheduler, api_requests, shutdown }.run().await;
    }

    /// Priority-based event loop: shutdown > API request > bridge event.
    async fn run(mut self) {
        loop {
            // Biased select ensures priority ordering: we always check shutdown first, then
            // API requests, and finally new bridge events.
            tokio::select! {
                biased;

                // Highest priority: exit the loop immediately on shutdown signal.
                () = self.shutdown.wait() => break,

                // Execute an API request closure against the scheduler.
                Some(api_request) = self.api_requests.recv() => {
                    api_request(&mut self.scheduler);
                }

                // Process the next L1 bridge event (new block, rollback, etc.).
                event = self.bridge.wait_and_pop() => {
                    if !self.handle_event(event) {
                        break;
                    }
                }
            }
        }

        // Shutdown sequence: stop the bridge first (no more events), then the scheduler.
        self.bridge.shutdown();
        self.scheduler.shutdown();
    }

    /// Dispatches a single bridge event. Returns `false` if the event loop should exit.
    fn handle_event(&mut self, event: L1Event) -> bool {
        match event {
            L1Event::Connected => {
                log::info!("L1 bridge connected");
            }

            L1Event::Disconnected => {
                log::warn!("L1 bridge disconnected");
            }

            L1Event::ChainBlockAdded { checkpoint, accepted_transactions, .. } => {
                let txs = accepted_transactions
                    .into_iter()
                    .map(|tx| {
                        let (resources, l2_payload) =
                            SchedulerTransaction::<L1Transaction>::extract_payload(&tx.payload);
                        SchedulerTransaction { tx, resources, l2_payload }
                    })
                    .collect();
                self.scheduler.schedule(*checkpoint.metadata(), txs);
            }

            L1Event::Rollback { checkpoint, .. } => {
                // Roll back the scheduler's committed state to the target index.
                if let Err(e) = self.scheduler.rollback_to(checkpoint.index()) {
                    log::error!("rollback to {} failed: {e}", checkpoint.index());
                    return false;
                }
            }

            L1Event::Finalized(block) => {
                // Advance the pruning threshold - state below this index can be pruned.
                self.scheduler.pruning().set_threshold(block.index());
            }

            L1Event::Fatal { reason } => {
                log::error!("L1 bridge fatal error: {reason}");
                return false;
            }
        }

        true
    }
}
