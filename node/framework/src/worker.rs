use std::{
    collections::VecDeque,
    sync::Arc,
    thread::{self, JoinHandle},
};

use tokio::runtime::Builder;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_node_l1_bridge::{BlockHash, L1Bridge, L1Event};
use vprogs_scheduling_scheduler::Scheduler;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{NodeVm, PreparedBatch};

/// A pending block in the event loop queue.
struct PendingBlock<V: NodeVm> {
    prepared: Arc<PreparedBatch<V>>,
    hash: BlockHash,
}

/// Dedicated event loop thread that connects the L1 bridge to the scheduler.
///
/// Consumes [`L1Event`]s from the bridge, dispatches pre-processing to execution workers, and feeds
/// completed batches to the scheduler in order. Runs on a dedicated thread with a single-threaded
/// tokio runtime (same pattern as [`L1Bridge`] and [`BatchLifecycleWorker`]).
pub(crate) struct NodeWorker<S: Store<StateSpace = StateSpace>, V: NodeVm> {
    shutdown: Arc<AtomicAsyncLatch>,
    handle: Option<JoinHandle<()>>,
    _marker: std::marker::PhantomData<(S, V)>,
}

impl<S: Store<StateSpace = StateSpace>, V: NodeVm> NodeWorker<S, V> {
    /// Spawns the event loop thread.
    pub(crate) fn new(scheduler: Scheduler<S, V>, bridge: L1Bridge, vm: V) -> Self {
        let shutdown = Arc::new(AtomicAsyncLatch::new());

        let handle = thread::spawn({
            let shutdown = shutdown.clone();
            move || {
                Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime")
                    .block_on(event_loop(scheduler, bridge, vm, shutdown));
            }
        });

        Self { shutdown, handle: Some(handle), _marker: std::marker::PhantomData }
    }

    /// Signals the event loop to stop and waits for the thread to finish.
    pub(crate) fn shutdown(&mut self) {
        self.shutdown.open();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("node worker panicked");
        }
    }
}

/// The async event loop that processes bridge events.
async fn event_loop<S: Store<StateSpace = StateSpace>, V: NodeVm>(
    mut scheduler: Scheduler<S, V>,
    bridge: L1Bridge,
    vm: V,
    shutdown: Arc<AtomicAsyncLatch>,
) {
    let mut queue: VecDeque<PendingBlock<V>> = VecDeque::new();

    loop {
        // Phase 1: Drain all ready batches from the front of the queue (non-blocking).
        while let Some(front) = queue.front() {
            if !front.prepared.is_ready() {
                break;
            }
            let pending = queue.pop_front().unwrap();
            let txs = pending.prepared.take_txs();
            scheduler.schedule(vm.batch_metadata(pending.hash), txs);
        }

        // Phase 2: select! between shutdown, front batch ready, or bridge event.
        tokio::select! {
            biased;

            () = shutdown.wait() => break,

            () = async {
                if let Some(front) = queue.front() {
                    front.prepared.wait().await;
                } else {
                    // Queue is empty — this branch should never fire.
                    std::future::pending::<()>().await;
                }
            } => continue,

            event = bridge.wait_and_pop() => {
                if !handle_event(event, &mut scheduler, &vm, &mut queue) {
                    break;
                }
            }
        }
    }

    // Shutdown sequence: stop bridge first, then scheduler.
    bridge.shutdown();
    scheduler.shutdown();
}

/// Dispatches a single bridge event. Returns `false` if the event loop should exit.
fn handle_event<S: Store<StateSpace = StateSpace>, V: NodeVm>(
    event: L1Event,
    scheduler: &mut Scheduler<S, V>,
    vm: &V,
    queue: &mut VecDeque<PendingBlock<V>>,
) -> bool {
    match event {
        L1Event::Connected => {
            log::info!("L1 bridge connected");
        }

        L1Event::Disconnected => {
            log::warn!("L1 bridge disconnected");
        }

        L1Event::ChainBlockAdded { index, header, accepted_transactions } => {
            let prepared = PreparedBatch::new(index);

            // Extract hash from the header before moving it into the closure.
            let hash = header.hash.unwrap_or_default();

            // Submit pre-processing to an execution worker.
            scheduler.submit_function({
                let vm = vm.clone();
                let prepared = prepared.clone();
                move || {
                    let txs = vm.pre_process_block(index, &header, &accepted_transactions);
                    prepared.set_txs(txs);
                }
            });

            queue.push_back(PendingBlock { prepared, hash });
        }

        L1Event::Rollback { index, .. } => {
            // Pop pending batches from the back where index > target.
            while let Some(back) = queue.back() {
                if back.prepared.index() > index {
                    queue.pop_back();
                } else {
                    break;
                }
            }
            scheduler.rollback_to(index);
        }

        L1Event::Finalized(block) => {
            scheduler.set_pruning_threshold(block.index());
        }

        L1Event::Fatal { reason } => {
            log::error!("L1 bridge fatal error: {reason}");
            return false;
        }
    }

    true
}
