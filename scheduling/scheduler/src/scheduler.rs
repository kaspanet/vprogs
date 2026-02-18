use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use tap::Tap;
use vprogs_core_types::{AccessMetadata, Checkpoint, Transaction};
use vprogs_scheduling_execution_workers::ExecutionWorkers;
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_types::Store;

use crate::{
    BatchLifecycleWorker, CancellationContext, ExecutionConfig, PruningWorker, Resource,
    ResourceAccess, RuntimeBatch, RuntimeBatchRef, RuntimeTxRef, SchedulerError, SchedulerResult,
    StateDiff, Write, cpu_task::ManagerTask, rollback::Rollback, state::SchedulerState,
    vm_interface::VmInterface,
};

/// Orchestrates transaction execution, state management, and storage coordination.
///
/// The scheduler is the main entry point for batch processing. It schedules transactions, manages
/// resource dependency chains, coordinates parallel execution via worker threads, and handles
/// rollbacks when chain reorganization occurs.
pub struct Scheduler<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// The VM implementation used to execute transactions.
    vm: V,
    /// Shared scheduler state (storage, eviction_queue, root, last_committed, last_processed).
    state: SchedulerState<S, V>,
    /// Shared cancellation state for in-flight batch detection.
    cancellation: CancellationContext,
    /// Checkpoints for batches that have not yet committed, in index order.
    pending_batches: VecDeque<Checkpoint<V::BatchMetadata>>,
    /// Maps resource IDs to their in-memory dependency chain heads.
    resources: HashMap<V::ResourceId, Resource<S, V>>,
    /// Background worker that processes batches through their lifecycle stages.
    batch_lifecycle_worker: BatchLifecycleWorker<S, V>,
    /// Thread pool for parallel transaction execution.
    execution_workers: ExecutionWorkers<ManagerTask<S, V>, RuntimeBatch<S, V>>,
    /// Background worker that prunes old state data when the pruning threshold advances.
    pruning_worker: PruningWorker<S, V>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> Scheduler<S, V> {
    /// Creates a new scheduler with the given execution and storage configurations.
    pub fn new(execution_config: ExecutionConfig<V>, storage_config: StorageConfig<S>) -> Self {
        let (worker_count, vm) = execution_config.unpack();
        let state = SchedulerState::new(storage_config);
        Self {
            batch_lifecycle_worker: BatchLifecycleWorker::new(vm.clone()),
            pruning_worker: PruningWorker::new(state.clone()),
            execution_workers: ExecutionWorkers::new(worker_count),
            resources: HashMap::new(),
            pending_batches: VecDeque::new(),
            cancellation: CancellationContext::new(state.root().index()),
            state,
            vm,
        }
    }

    /// Schedules a batch of transactions for execution.
    ///
    /// Creates a new `RuntimeBatch`, connects its transactions to resource dependency chains,
    /// pushes it to the worker loop for lifecycle management, and submits it to execution workers
    /// for parallel processing. After building resource accesses, processes pending eviction
    /// requests to clean up resources from committed batches.
    pub fn schedule(
        &mut self,
        metadata: V::BatchMetadata,
        txs: Vec<V::Transaction>,
    ) -> RuntimeBatch<S, V> {
        let checkpoint = self.next_checkpoint(metadata);

        RuntimeBatch::new(self, txs, checkpoint)
            // Connect transactions to resource dependency chains.
            .tap(RuntimeBatch::connect)
            .tap(|batch| {
                // Push to the batch lifecycle worker for lifecycle progression.
                self.batch_lifecycle_worker.push(batch.clone());

                // Submit to execution workers for parallel processing.
                self.execution_workers.execute(batch.clone());

                // Process eviction queue after scheduling to avoid race conditions.
                // Resources touched by this batch will have updated last_access and won't be
                // evicted.
                self.process_eviction_queue()
            })
    }

    /// Processes pending eviction requests from committed batches.
    ///
    /// For each resource ID in the eviction queue, checks if its last access belongs to a committed
    /// batch. If so, removes the resource from the cache. Resources that were accessed by a pending
    /// batch (including the one just scheduled) will have an uncommitted last access and be
    /// skipped.
    pub fn process_eviction_queue(&mut self) {
        while let Some(resource_id) = self.state.eviction_queue().pop() {
            if let Some(resource) = self.resources.get(&resource_id) {
                if resource.should_evict() {
                    self.resources.remove(&resource_id);
                }
            }
        }
    }

    /// Rolls back the runtime state to the given batch index, returning the target checkpoint. If
    /// the current state is already at or behind the target, returns the current checkpoint.
    ///
    /// Returns [`SchedulerError::PruningConflict`] if pruning has advanced past the rollback target
    /// and the required rollback pointers have been deleted.
    pub fn rollback_to(
        &mut self,
        target_index: u64,
    ) -> SchedulerResult<Checkpoint<V::BatchMetadata>> {
        // Determine the range of batches to roll back.
        let upper_bound = self.state.last_processed().index();

        // Only perform a rollback if there is state to revert.
        if upper_bound > target_index {
            // Prevent the pruning worker from pruning into the rollback range.
            // Returns false if pruning has already advanced past the target.
            if !self.pruning_worker.pause(target_index) {
                return Err(SchedulerError::PruningConflict);
            }

            // Look up target metadata, cancel in-flight batches, and update shared state.
            let target = self.cancel_and_rollback(target_index);

            // Submit the rollback command and wait for its completion.
            let done_signal = Default::default();
            self.state.storage().submit_write(Write::Rollback(Rollback::new(
                target.clone(),
                upper_bound,
                self.state.clone(),
                &done_signal,
            )));
            done_signal.wait_blocking();

            // Rollback complete — allow pruning to resume.
            self.pruning_worker.unpause();

            // Clear in-memory resource pointers, as their state may no longer be valid.
            self.resources.clear();

            Ok(target)
        } else {
            Ok((*self.state.last_processed()).clone())
        }
    }

    /// Returns a reference to the shared scheduler state.
    pub fn state(&self) -> &SchedulerState<S, V> {
        &self.state
    }

    /// Returns a reference to the VM implementation.
    pub fn vm(&self) -> &V {
        &self.vm
    }

    /// Submits a standalone function for execution on a worker thread.
    ///
    /// The function is injected into the global task queue and picked up by the next available
    /// execution worker as a last-resort fallback in the steal chain.
    pub fn submit_function(&self, func: impl FnOnce() + Send + Sync + 'static) {
        self.execution_workers.submit_task(ManagerTask::ExecuteFunction(Box::new(func)));
    }

    /// Returns the number of resources currently cached in memory.
    pub fn cached_resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Returns a reference to the pruning worker.
    pub fn pruning(&self) -> &PruningWorker<S, V> {
        &self.pruning_worker
    }

    /// Shuts down the scheduler and all its components.
    ///
    /// This stops the pruning worker, batch lifecycle worker, execution workers, and storage
    /// manager in order.
    pub fn shutdown(self) {
        self.pruning_worker.shutdown();
        self.batch_lifecycle_worker.shutdown();
        self.execution_workers.shutdown();
        self.state.storage().shutdown();
    }

    /// Returns a reference to the cancellation context.
    pub(crate) fn cancellation(&self) -> &CancellationContext {
        &self.cancellation
    }

    /// Builds resource accesses for a transaction by linking it into dependency chains.
    ///
    /// For each resource the transaction accesses, this either creates a new dependency chain or
    /// appends the transaction to an existing one. When a transaction is the first in its batch to
    /// access a resource, a new state diff is created and added to `state_diffs`.
    pub(crate) fn resources(
        &mut self,
        tx: &V::Transaction,
        runtime_tx: RuntimeTxRef<S, V>,
        batch: &RuntimeBatchRef<S, V>,
        state_diffs: &mut Vec<StateDiff<S, V>>,
    ) -> Vec<ResourceAccess<S, V>> {
        tx.accessed_resources()
            .iter()
            .map(|access| {
                // Get or create the resource entry and link this transaction into its chain.
                self.resources
                    .entry(access.id())
                    .or_default()
                    .access(access, &runtime_tx, batch)
                    .tap(|access| {
                        // If this is the first access in the batch, create a state diff.
                        if access.is_batch_head() {
                            state_diffs.push(access.state_diff());
                        }
                    })
            })
            .collect()
    }

    /// Advances to the next batch, returning its checkpoint.
    fn next_checkpoint(&mut self, metadata: V::BatchMetadata) -> Checkpoint<V::BatchMetadata> {
        self.drain_committed();

        let checkpoint = Checkpoint::new(self.state.last_processed().index() + 1, metadata);
        self.state.set_last_processed(Arc::new(checkpoint.clone()));
        self.pending_batches.push_back(checkpoint.clone());

        // Initialize root when the first batch is scheduled. On a fresh database or after
        // rollback-to-genesis, root is default (index 0). The disk write is deferred to
        // commit() for crash-fault tolerance.
        if self.state.root().index() == 0 {
            self.state.set_root(Arc::new(checkpoint.clone()));
        }

        checkpoint
    }

    /// Cancels in-flight batches and rolls back shared state to the given index.
    fn cancel_and_rollback(&mut self, target_index: u64) -> Checkpoint<V::BatchMetadata> {
        let target = self.lookup_checkpoint(target_index);

        // Cancel in-flight batches first so `commit_done()` sees the cancellation before
        // we update shared state.
        self.cancellation.rollback(target_index);

        // Update last_processed. last_committed is corrected by Rollback::execute() on the
        // write worker, which sees the true committed state without races.
        self.state.set_last_processed(Arc::new(target.clone()));

        // Pop canceled entries from the tip. Must happen after lookup_checkpoint (which
        // searches the pending queue) but before returning.
        while self.pending_batches.back().is_some_and(|cp| cp.index() > target_index) {
            self.pending_batches.pop_back();
        }

        target
    }

    /// Drains committed entries from the front of the pending batch queue.
    fn drain_committed(&mut self) {
        let committed = self.state.last_committed().index();
        while self.pending_batches.front().is_some_and(|cp| cp.index() <= committed) {
            self.pending_batches.pop_front();
        }
    }

    /// Looks up a checkpoint by index, searching the pending batch queue first, then disk.
    fn lookup_checkpoint(&self, index: u64) -> Checkpoint<V::BatchMetadata> {
        // Index 0 is the genesis state — no batch exists on disk for it.
        if index == 0 {
            return Checkpoint::default();
        }

        // Search pending batch queue (sorted ascending by index).
        for cp in &self.pending_batches {
            if cp.index() == index {
                return cp.clone();
            } else if cp.index() > index {
                break;
            }
        }

        // Fall back to disk for committed batches.
        Checkpoint::new(index, StoredBatchMetadata::get(&**self.state.storage().store(), index))
    }
}
