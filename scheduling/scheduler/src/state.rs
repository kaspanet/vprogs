use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};
use crossbeam_queue::SegQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::{Checkpoint, ResourceId};
use vprogs_state_metadata::StateMetadata;
use vprogs_storage_manager::{StorageConfig, StorageManager};
use vprogs_storage_types::Store;

use crate::{
    Read, ScheduledBatch, ScheduledBatchRef, Write, processor::Processor,
    scheduled_batch::ScheduledBatchData,
};

/// Shared scheduler state accessible by all components.
#[smart_pointer]
pub struct SchedulerState<S: Store, P: Processor<S>> {
    /// Storage manager for read/write coordination with background workers.
    storage: StorageManager<S, Read<S, P>, Write<S, P>>,
    /// Queue of resource IDs to potentially evict after their batches committed.
    eviction_queue: SegQueue<ResourceId>,
    /// Oldest surviving batch. Advanced by pruning.
    root: ArcSwap<Checkpoint<P::BatchMetadata>>,
    /// Most recently committed batch. Advanced by `commit_done`, reset on rollback.
    last_committed: ArcSwap<Checkpoint<P::BatchMetadata>>,
    /// Most recently scheduled batch. Advanced by `next_checkpoint`, reset on rollback. Only
    /// mutated from `&mut Scheduler`.
    last_processed: ArcSwap<Checkpoint<P::BatchMetadata>>,
    /// Head of the batch chain. Advanced by `schedule`, reset on rollback. Only mutated from
    /// `&mut Scheduler`.
    last_batch: ArcSwapOption<ScheduledBatchData<S, P>>,
}

impl<S: Store, P: Processor<S>> SchedulerState<S, P> {
    /// Creates a new state from a storage configuration.
    ///
    /// `root` and `last_committed` are loaded from `StateMetadata`. `last_processed` is initialized
    /// to `last_committed` since no new batches have been scheduled yet.
    pub fn new(storage_config: StorageConfig<S>) -> Self {
        let storage = StorageManager::new(storage_config);
        let root: Checkpoint<P::BatchMetadata> = StateMetadata::root(storage.store().as_ref());
        let last_committed: Checkpoint<P::BatchMetadata> =
            StateMetadata::last_committed(storage.store().as_ref());

        Self(Arc::new(SchedulerStateData {
            storage,
            eviction_queue: SegQueue::new(),
            root: ArcSwap::from_pointee(root),
            last_committed: ArcSwap::from_pointee(last_committed.clone()),
            last_processed: ArcSwap::from_pointee(last_committed),
            last_batch: ArcSwapOption::empty(),
        }))
    }

    /// Returns a reference to the storage manager.
    pub fn storage(&self) -> &StorageManager<S, Read<S, P>, Write<S, P>> {
        &self.storage
    }

    /// Returns a reference to the eviction queue.
    pub fn eviction_queue(&self) -> &SegQueue<ResourceId> {
        &self.eviction_queue
    }

    /// Returns the root checkpoint (oldest surviving batch).
    pub fn root(&self) -> Arc<Checkpoint<P::BatchMetadata>> {
        self.root.load_full()
    }

    /// Returns the most recently committed checkpoint.
    pub fn last_committed(&self) -> Arc<Checkpoint<P::BatchMetadata>> {
        self.last_committed.load_full()
    }

    /// Returns the most recently processed (scheduled) checkpoint.
    pub fn last_processed(&self) -> Arc<Checkpoint<P::BatchMetadata>> {
        self.last_processed.load_full()
    }

    /// Returns the head of the batch chain, if any.
    pub fn last_batch(&self) -> Option<ScheduledBatch<S, P>> {
        self.last_batch.load_full().map(ScheduledBatch)
    }

    /// Returns a reference to the head of the batch chain, or a default if none exists.
    pub fn last_batch_ref(&self) -> ScheduledBatchRef<S, P> {
        self.last_batch().map_or_else(ScheduledBatchRef::default, |b| b.downgrade())
    }

    /// Walks the batch chain backwards to find the batch at the given index.
    pub fn batch(&self, index: u64) -> Option<ScheduledBatch<S, P>> {
        let mut cursor = self.last_batch();
        loop {
            match cursor {
                Some(batch) if batch.checkpoint().index() > index => {
                    cursor = batch.prev().upgrade();
                }
                Some(batch) if batch.checkpoint().index() == index => break Some(batch),
                _ => break None,
            }
        }
    }

    /// Sets the root checkpoint.
    pub(crate) fn set_root(&self, checkpoint: Arc<Checkpoint<P::BatchMetadata>>) {
        self.root.store(checkpoint);
    }

    /// Sets the most recently committed checkpoint.
    pub(crate) fn set_last_committed(&self, checkpoint: Arc<Checkpoint<P::BatchMetadata>>) {
        self.last_committed.store(checkpoint);
    }

    /// Sets the most recently processed (scheduled) checkpoint.
    pub(crate) fn set_last_processed(&self, checkpoint: Arc<Checkpoint<P::BatchMetadata>>) {
        self.last_processed.store(checkpoint);
    }

    /// Sets the head of the batch chain.
    pub(crate) fn set_last_batch(&self, batch: Option<ScheduledBatch<S, P>>) {
        self.last_batch.store(batch.map(|b| b.0));
    }
}
