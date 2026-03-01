use std::sync::Arc;

use arc_swap::ArcSwap;
use crossbeam_queue::SegQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::Checkpoint;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::{StorageConfig, StorageManager};
use vprogs_storage_types::Store;

use crate::{Read, Write, vm_interface::VmInterface};

/// Shared scheduler state accessible by all components.
#[smart_pointer]
pub struct SchedulerState<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// Storage manager for read/write coordination with background workers.
    storage: StorageManager<S, Read<S, V>, Write<S, V>>,
    /// Queue of resource IDs to potentially evict after their batches committed.
    eviction_queue: SegQueue<V::ResourceId>,
    /// Oldest surviving batch. Advanced by pruning.
    root: ArcSwap<Checkpoint<V::BatchMetadata>>,
    /// Most recently committed batch. Advanced by `commit_done`, reset on rollback.
    last_committed: ArcSwap<Checkpoint<V::BatchMetadata>>,
    /// Most recently scheduled batch. Advanced by `next_checkpoint`, reset on rollback. Only
    /// mutated from `&mut Scheduler`.
    last_processed: ArcSwap<Checkpoint<V::BatchMetadata>>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> SchedulerState<S, V> {
    /// Creates a new state from a storage configuration.
    ///
    /// `root` and `last_committed` are loaded from `StateMetadata`. `last_processed` is initialized
    /// to `last_committed` since no new batches have been scheduled yet.
    pub fn new(storage_config: StorageConfig<S>) -> Self {
        let storage = StorageManager::new(storage_config);
        let root: Checkpoint<V::BatchMetadata> = StateMetadata::root(storage.store().as_ref());
        let last_committed: Checkpoint<V::BatchMetadata> =
            StateMetadata::last_committed(storage.store().as_ref());

        Self(Arc::new(SchedulerStateData {
            storage,
            eviction_queue: SegQueue::new(),
            root: ArcSwap::from_pointee(root),
            last_committed: ArcSwap::from_pointee(last_committed.clone()),
            last_processed: ArcSwap::from_pointee(last_committed),
        }))
    }

    /// Returns a reference to the storage manager.
    pub fn storage(&self) -> &StorageManager<S, Read<S, V>, Write<S, V>> {
        &self.storage
    }

    /// Returns a reference to the eviction queue.
    pub fn eviction_queue(&self) -> &SegQueue<V::ResourceId> {
        &self.eviction_queue
    }

    /// Returns the root checkpoint (oldest surviving batch).
    pub fn root(&self) -> Arc<Checkpoint<V::BatchMetadata>> {
        self.root.load_full()
    }

    /// Returns the most recently committed checkpoint.
    pub fn last_committed(&self) -> Arc<Checkpoint<V::BatchMetadata>> {
        self.last_committed.load_full()
    }

    /// Returns the most recently processed (scheduled) checkpoint.
    pub fn last_processed(&self) -> Arc<Checkpoint<V::BatchMetadata>> {
        self.last_processed.load_full()
    }

    /// Sets the root checkpoint.
    pub(crate) fn set_root(&self, checkpoint: Arc<Checkpoint<V::BatchMetadata>>) {
        self.root.store(checkpoint);
    }

    /// Sets the most recently committed checkpoint.
    pub(crate) fn set_last_committed(&self, checkpoint: Arc<Checkpoint<V::BatchMetadata>>) {
        self.last_committed.store(checkpoint);
    }

    /// Sets the most recently processed (scheduled) checkpoint.
    pub(crate) fn set_last_processed(&self, checkpoint: Arc<Checkpoint<V::BatchMetadata>>) {
        self.last_processed.store(checkpoint);
    }
}
