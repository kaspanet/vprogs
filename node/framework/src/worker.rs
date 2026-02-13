use std::{collections::VecDeque, future::pending, sync::Arc};

use tokio::sync::mpsc;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_node_l1_bridge::{L1Bridge, L1Event};
use vprogs_scheduling_scheduler::Scheduler;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{NodeVm, api::ApiRequest, batch::Batch};

/// Owns the scheduler and bridge, processing L1 events through the L2 scheduler.
///
/// Consumes [`L1Event`]s from the bridge, dispatches pre-processing to execution workers, and feeds
/// completed batches to the scheduler in order.
pub(crate) struct NodeWorker<S: Store<StateSpace = StateSpace>, V: NodeVm> {
    /// L1 chain event source - the primary data entry point into the node.
    bridge: L1Bridge,
    /// L2 transaction scheduler and execution engine.
    scheduler: Scheduler<S, V>,
    /// Incoming [`NodeApi`](crate::NodeApi) requests.
    api_requests: mpsc::Receiver<ApiRequest<S, V>>,
    /// Batches awaiting pre-processing completion, ordered by block index.
    pending_batches: VecDeque<Arc<Batch<V>>>,
    /// Signal to stop the event loop.
    shutdown: Arc<AtomicAsyncLatch>,
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> NodeWorker<S, V> {
    /// Creates the worker and runs the event loop until shutdown is signaled or a fatal error
    /// occurs. Shuts down the bridge and scheduler before returning.
    pub(crate) async fn spawn(
        bridge: L1Bridge,
        scheduler: Scheduler<S, V>,
        api_requests: mpsc::Receiver<ApiRequest<S, V>>,
        shutdown: Arc<AtomicAsyncLatch>,
    ) {
        let pending_batches = VecDeque::new();
        Self { bridge, scheduler, api_requests, pending_batches, shutdown }.run().await;
    }

    /// Priority-based event loop: shutdown > next batch prepared > API request > bridge event.
    async fn run(mut self) {
        loop {
            // Feed all consecutively prepared batches into the scheduler before blocking.
            self.process_prepared_batches();

            // Biased select ensures priority ordering: we always check shutdown first, then
            // whether the next in-order batch has finished pre-processing, then API requests,
            // and finally new bridge events.
            tokio::select! {
                biased;

                // Highest priority: exit the loop immediately on shutdown signal.
                () = self.shutdown.wait() => break,

                // A pending batch just finished pre-processing - loop back to process it.
                () = async {
                    if let Some(batch) = self.pending_batches.front() {
                        batch.wait_until_prepared().await;
                    } else {
                        // No pending batches - pend forever to disable this branch.
                        pending::<()>().await;
                    }
                } => continue,

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

    /// Processes all prepared batches from the front of the queue and schedules them.
    ///
    /// Stops at the first unprepared batch to preserve block ordering.
    fn process_prepared_batches(&mut self) {
        while let Some(batch) = self.pending_batches.front() {
            if !batch.is_prepared() {
                // Next batch is still being pre-processed - stop here to preserve ordering.
                break;
            }

            // Take the prepared batch out of the queue and extract its results.
            let batch = self.pending_batches.pop_front().expect("batch should exist");
            let (txs, metadata) = batch.take();

            // Feed the transactions into the scheduler for parallel execution.
            self.scheduler.schedule(metadata, txs);
        }
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

            L1Event::ChainBlockAdded { checkpoint, header, accepted_transactions } => {
                let index = checkpoint.index();
                let batch = Batch::new(index);

                // Submit pre-processing to an execution worker. The closure captures the VM
                // and batch handle so it can run independently on the thread pool.
                self.scheduler.submit_function({
                    let vm = self.scheduler.vm().clone();
                    let batch = batch.clone();
                    move || {
                        let txs = vm.pre_process_block(index, &header, &accepted_transactions);
                        batch.set(txs, *checkpoint.metadata());
                    }
                });

                // Enqueue the batch - it will be processed once pre-processing completes.
                self.pending_batches.push_back(batch);
            }

            L1Event::Rollback { checkpoint, .. } => {
                // Discard any in-flight batches beyond the rollback target. These may still
                // be pre-processing on worker threads but their results will be ignored.
                while let Some(batch) = self.pending_batches.back() {
                    if batch.index() > checkpoint.index() {
                        self.pending_batches.pop_back();
                    } else {
                        break;
                    }
                }

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
