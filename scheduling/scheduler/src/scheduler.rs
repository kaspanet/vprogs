use std::{collections::HashMap, sync::Arc};

use crossbeam_queue::SegQueue;
use tap::Tap;
use vprogs_core_types::{AccessMetadata, Checkpoint, Transaction};
use vprogs_scheduling_execution_workers::ExecutionWorkers;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::{StorageConfig, StorageManager};
use vprogs_storage_types::Store;

use crate::{
    BatchExecutionContext, BatchLifecycleWorker, ExecutionConfig, PruningWorker, Read, Resource,
    ResourceAccess, Rollback, RuntimeBatch, RuntimeBatchRef, RuntimeTxRef, StateDiff, Write,
    cpu_task::ManagerTask, vm_interface::VmInterface,
};

/// Orchestrates transaction execution, state management, and storage coordination.
///
/// The scheduler is the main entry point for batch processing. It schedules transactions, manages
/// resource dependency chains, coordinates parallel execution via worker threads, and handles
/// rollbacks when chain reorganization occurs.
pub struct Scheduler<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// The VM implementation used to execute transactions.
    vm: V,
    /// Tracks runtime state such as batch indices, cached checkpoints, and cancellation states.
    batch_execution: BatchExecutionContext<S, V::BatchMetadata>,
    /// Handles persistence of state diffs and rollback operations.
    storage: StorageManager<S, Read<S, V>, Write<S, V>>,
    /// Maps resource IDs to their in-memory dependency chain heads.
    resources: HashMap<V::ResourceId, Resource<S, V>>,
    /// Background worker that processes batches through their lifecycle stages.
    batch_lifecycle_worker: BatchLifecycleWorker<S, V>,
    /// Thread pool for parallel transaction execution.
    execution_workers: ExecutionWorkers<ManagerTask<S, V>, RuntimeBatch<S, V>>,
    /// Queue of resource IDs to potentially evict after their batches committed.
    eviction_queue: Arc<SegQueue<V::ResourceId>>,
    /// Background worker that prunes old state data when the pruning threshold advances.
    pruning_worker: PruningWorker<S, V>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> Scheduler<S, V> {
    /// Creates a new scheduler with the given execution and storage configurations.
    pub fn new(execution_config: ExecutionConfig<V>, storage_config: StorageConfig<S>) -> Self {
        let storage = StorageManager::new(storage_config);
        let (worker_count, vm) = execution_config.unpack();
        Self {
            batch_execution: BatchExecutionContext::new(storage.store().clone()),
            batch_lifecycle_worker: BatchLifecycleWorker::new(vm.clone()),
            pruning_worker: PruningWorker::new(storage.store().clone()),
            resources: HashMap::new(),
            execution_workers: ExecutionWorkers::new(worker_count),
            eviction_queue: Arc::new(SegQueue::new()),
            storage,
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
        // Advance the batch sequence and obtain the shared state needed by the runtime batch:
        // the checkpoint identity, cancellation context for rollback detection, and the atomic
        // commit frontier that workers advance when a batch commits.
        let (checkpoint, cancel, commit) = self.batch_execution.next_checkpoint(metadata);

        RuntimeBatch::new(self.vm.clone(), self, txs, checkpoint, cancel, commit)
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
        while let Some(resource_id) = self.eviction_queue.pop() {
            if let Some(resource) = self.resources.get(&resource_id) {
                if resource.should_evict() {
                    self.resources.remove(&resource_id);
                }
            }
        }
    }

    /// Rolls back the runtime state to the given batch index, returning the target checkpoint.
    /// If the current state is already at or behind the target, returns the current checkpoint.
    pub fn rollback_to(&mut self, target_index: u64) -> Checkpoint<V::BatchMetadata> {
        // Determine the range of batches to roll back.
        let upper_bound = self.batch_execution.last_processed().index();

        // Only perform a rollback if there is state to revert.
        if upper_bound > target_index {
            // Look up target metadata and update batch execution context.
            let (target, commit_frontier) = self.batch_execution.rollback(target_index);

            // Submit the rollback command and wait for its completion.
            let done_signal = Default::default();
            self.storage.submit_write(Write::Rollback(Rollback::new(
                target.clone(),
                upper_bound,
                commit_frontier,
                &done_signal,
            )));
            done_signal.wait_blocking();

            // Clear in-memory resource pointers, as their state may no longer be valid.
            self.resources.clear();

            target
        } else {
            self.batch_execution.last_processed().clone()
        }
    }

    /// Returns a reference to the batch execution context.
    pub fn batch_execution(&self) -> &BatchExecutionContext<S, V::BatchMetadata> {
        &self.batch_execution
    }

    /// Returns a reference to the VM implementation.
    pub fn vm(&self) -> &V {
        &self.vm
    }

    /// Returns a reference to the storage manager.
    pub fn storage(&self) -> &StorageManager<S, Read<S, V>, Write<S, V>> {
        &self.storage
    }

    /// Submits a standalone function for execution on a worker thread.
    ///
    /// The function is injected into the global task queue and picked up by the next available
    /// execution worker as a last-resort fallback in the steal chain.
    pub fn submit_function(&self, func: impl FnOnce() + Send + Sync + 'static) {
        self.execution_workers.submit_task(ManagerTask::ExecuteFunction(Box::new(func)));
    }

    /// Returns a clone of the eviction queue.
    pub fn eviction_queue(&self) -> Arc<SegQueue<V::ResourceId>> {
        self.eviction_queue.clone()
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
        self.storage.shutdown();
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
}
