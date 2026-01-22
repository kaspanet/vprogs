use std::{collections::HashMap, sync::Arc};

use crossbeam_queue::SegQueue;
use tap::Tap;
use vprogs_core_types::{AccessMetadata, Transaction};
use vprogs_scheduling_execution_workers::ExecutionWorkers;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::{StorageConfig, StorageManager};
use vprogs_storage_types::Store;

use crate::{
    ExecutionConfig, Read, Resource, ResourceAccess, Rollback, RuntimeBatch, RuntimeBatchRef,
    RuntimeContext, RuntimeTxRef, StateDiff, WorkerLoop, Write, cpu_task::ManagerTask,
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
    /// Tracks runtime state such as batch indices and cancellation states.
    context: RuntimeContext,
    /// Handles persistence of state diffs and rollback operations.
    storage_manager: StorageManager<S, Read<S, V>, Write<S, V>>,
    /// Maps resource IDs to their in-memory dependency chain heads.
    resources: HashMap<V::ResourceId, Resource<S, V>>,
    /// Background loop that processes batches through their lifecycle stages.
    worker_loop: WorkerLoop<S, V>,
    /// Thread pool for parallel transaction execution.
    execution_workers: ExecutionWorkers<ManagerTask<S, V>, RuntimeBatch<S, V>>,
    /// Queue of resource IDs to potentially evict after their batches commit.
    eviction_queue: Arc<SegQueue<V::ResourceId>>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> Scheduler<S, V> {
    /// Creates a new scheduler with the given execution and storage configurations.
    pub fn new(execution_config: ExecutionConfig<V>, storage_config: StorageConfig<S>) -> Self {
        let (worker_count, vm) = execution_config.unpack();
        Self {
            context: RuntimeContext::new(0),
            worker_loop: WorkerLoop::new(vm.clone()),
            storage_manager: StorageManager::new(storage_config),
            resources: HashMap::new(),
            execution_workers: ExecutionWorkers::new(worker_count),
            eviction_queue: Arc::new(SegQueue::new()),
            vm,
        }
    }

    /// Schedules a batch of transactions for execution.
    ///
    /// Creates a new `RuntimeBatch`, connects its transactions to resource dependency chains,
    /// pushes it to the worker loop for lifecycle management, and submits it to execution workers
    /// for parallel processing. After building resource accesses, processes pending eviction
    /// requests to clean up resources from committed batches.
    pub fn schedule(&mut self, txs: Vec<V::Transaction>) -> RuntimeBatch<S, V> {
        RuntimeBatch::new(self.vm.clone(), self, txs)
            // Connect transactions to resource dependency chains.
            .tap(RuntimeBatch::connect)
            .tap(|batch| {
                // Push to the worker loop for lifecycle progression.
                self.worker_loop.push(batch.clone());
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
    fn process_eviction_queue(&mut self) {
        while let Some(resource_id) = self.eviction_queue.pop() {
            if let Some(resource) = self.resources.get(&resource_id) {
                if resource.should_evict() {
                    self.resources.remove(&resource_id);
                }
            }
        }
    }

    /// Rolls back the runtime state to `target_index` if the current state is ahead of it.
    ///
    /// This updates the runtime context to reflect the rollback and submits a rollback command to
    /// the storage manager. The call blocks until the rollback completes, after which all in-memory
    /// resource pointers are cleared, as their state may have changed.
    pub fn rollback_to(&mut self, target_index: u64) {
        // Determine the range of batches to roll back.
        let lower_bound = target_index + 1;
        let upper_bound = self.context.last_batch_index();

        // Only perform a rollback if there is state to revert.
        if upper_bound >= lower_bound {
            // Update the context and cancels in-flight batches.
            self.context.rollback(target_index);

            // Submit the rollback command and wait for its completion.
            let done_signal = Default::default();
            self.storage_manager.submit_write(Write::Rollback(Rollback::new(
                lower_bound,
                upper_bound,
                &done_signal,
            )));
            done_signal.wait_blocking();

            // Clear in-memory resource pointers, as their state may no longer be valid.
            self.resources.clear();
        }
    }

    /// Returns a reference to the runtime context.
    pub fn context(&self) -> &RuntimeContext {
        &self.context
    }

    /// Returns a reference to the storage manager.
    pub fn storage_manager(&self) -> &StorageManager<S, Read<S, V>, Write<S, V>> {
        &self.storage_manager
    }

    /// Shuts down the scheduler and all its components.
    ///
    /// This stops the worker loop, execution workers, and storage manager in order.
    pub fn shutdown(self) {
        self.worker_loop.shutdown();
        self.execution_workers.shutdown();
        self.storage_manager.shutdown();
    }

    /// Returns a clone of the eviction queue for use by batches.
    pub(crate) fn eviction_queue(&self) -> Arc<SegQueue<V::ResourceId>> {
        self.eviction_queue.clone()
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
