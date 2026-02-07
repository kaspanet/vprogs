use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use arc_swap::ArcSwap;
use tap::Tap;
use tokio::{runtime::Builder, sync::Notify};
use vprogs_core_types::ResourceId;
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::Store;

use crate::VmInterface;

/// Background worker that processes pruning requests with direct store access.
///
/// The pruning worker monitors the pruning threshold and deletes old state data (rollback pointers
/// and their associated versions) for batches that will never be rolled back. This reclaims storage
/// space while preserving the ability to rollback to recent batches.
///
/// Pruning progress is persisted to the Metadata column family, making the process crash-fault
/// tolerant. On restart, the worker resumes from the last successfully pruned batch.
///
/// Unlike normal write operations, pruning runs on its own dedicated thread with direct store
/// access, avoiding contention with the main write path.
///
/// All coordination uses lock-free primitives:
/// - `AtomicU64` for threshold and progress tracking
/// - `Notify` for wakeup signaling
pub struct PruningWorker<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// The batch index up to which pruning is allowed (exclusive).
    /// Batches with index < threshold can be pruned.
    pruning_threshold: Arc<AtomicU64>,
    /// Cached last pruned batch (index + metadata). Updated atomically after each pruning pass
    /// to avoid disk reads on the query path.
    last_pruned: Arc<ArcSwap<(u64, V::BatchMetadata)>>,
    /// Notification signal to wake up the worker when the threshold changes.
    notify: Arc<Notify>,
    /// Handle to the background worker thread.
    handle: JoinHandle<()>,
    /// Marker for the store and VM interface types.
    _marker: PhantomData<(S, V)>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> PruningWorker<S, V> {
    /// Sets the pruning threshold.
    ///
    /// Batches with index < threshold become eligible for pruning. The actual pruning happens
    /// asynchronously in the background worker. Setting a threshold lower than the current value
    /// has no effect (pruning only moves forward).
    pub fn set_threshold(&self, threshold: u64) {
        // Only update if the new threshold is higher (pruning is monotonic).
        let current = self.pruning_threshold.load(Ordering::Acquire);
        if threshold > current {
            self.pruning_threshold.store(threshold, Ordering::Release);
            self.notify.notify_one();
        }
    }

    /// Returns the current pruning threshold.
    pub fn threshold(&self) -> u64 {
        self.pruning_threshold.load(Ordering::Acquire)
    }

    /// Returns the last successfully pruned batch (index and metadata) from cache.
    pub fn last_pruned(&self) -> (u64, V::BatchMetadata) {
        let cached = self.last_pruned.load();
        (cached.0, cached.1.clone())
    }

    /// Creates a new pruning worker with direct store access.
    ///
    /// The worker resumes from the last successfully pruned batch index stored in metadata.
    /// If no pruning has occurred yet, it starts from 0.
    pub(crate) fn new(store: Arc<S>) -> Self {
        // Load the last pruned state from persistent storage.
        let (persisted_index, persisted_metadata): (u64, V::BatchMetadata) =
            StateMetadata::last_pruned(store.as_ref());

        let pruning_threshold = Arc::new(AtomicU64::new(persisted_index));
        let last_pruned = Arc::new(ArcSwap::from_pointee((persisted_index, persisted_metadata)));
        let notify = Arc::new(Notify::new());

        let handle =
            Self::start(store, pruning_threshold.clone(), last_pruned.clone(), notify.clone());

        Self { pruning_threshold, last_pruned, notify, handle, _marker: PhantomData }
    }

    /// Shuts down the pruning worker and waits for it to complete.
    pub(crate) fn shutdown(self) {
        drop(self.pruning_threshold);
        self.notify.notify_one();
        self.handle.join().expect("pruning worker panicked");
    }

    /// Starts the background pruning worker thread.
    ///
    /// The worker monitors the pruning threshold and deletes old state data as needed.
    /// It uses direct store access to avoid contention with the main write path.
    /// The worker loop exits when the outer struct is dropped (detected via strong_count).
    fn start(
        store: Arc<S>,
        pruning_threshold: Arc<AtomicU64>,
        last_pruned: Arc<ArcSwap<(u64, V::BatchMetadata)>>,
        notify: Arc<Notify>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            Builder::new_current_thread().build().expect("failed to build tokio runtime").block_on(
                async move {
                    // Use strong_count to detect shutdown (when the outer struct is dropped).
                    while Arc::strong_count(&pruning_threshold) != 1 {
                        let threshold = pruning_threshold.load(Ordering::Acquire);
                        let last_pruned_index = last_pruned.load().0;

                        // Check if there's pruning work to do.
                        if threshold > last_pruned_index + 1 {
                            // Prune batches from (last_pruned + 1) to (threshold - 1) inclusive.
                            let lower_bound = last_pruned_index + 1;
                            let upper_bound = threshold - 1;

                            // Execute pruning directly on the store.
                            let metadata = Self::prune(&store, lower_bound, upper_bound);

                            // Update the cached last pruned state.
                            last_pruned.store(Arc::new((upper_bound, metadata)));
                        } else if Arc::strong_count(&pruning_threshold) != 1 {
                            // No work to do, wait for notification.
                            notify.notified().await;
                        }
                    }
                },
            )
        })
    }

    /// Executes pruning directly on the store for the given batch range.
    ///
    /// This runs on the dedicated pruning thread and does not interfere with the main write path.
    /// The last pruned index and batch id are persisted atomically with the deletions for
    /// crash-fault tolerance.
    fn prune(store: &S, lower_bound: u64, upper_bound: u64) -> V::BatchMetadata {
        let metadata: V::BatchMetadata = StoredBatchMetadata::get(store, upper_bound);

        // Commit all deletions and metadata update atomically.
        store.commit(store.write_batch().tap_mut(|wb| {
            // Persist pruning metadata upfront. This is committed atomically with the deletions
            // below for crash-fault tolerance.
            StateMetadata::set_last_pruned(wb, upper_bound, &metadata);

            // Walk batches from oldest to newest (order doesn't matter for pruning).
            for index in lower_bound..=upper_bound {
                // Delete all rollback pointers and their referenced old versions for this batch.
                for (resource_id_bytes, old_version) in StatePtrRollback::iter_batch(store, index) {
                    let resource_id = V::ResourceId::from_bytes(&resource_id_bytes);

                    // Delete the old version data if it exists (version 0 means resource didn't
                    // exist).
                    if old_version != 0 {
                        StateVersion::delete(wb, old_version, &resource_id);
                    }

                    // Delete the rollback pointer itself.
                    StatePtrRollback::delete(wb, index, &resource_id);
                }

                // Delete batch metadata entries for this batch.
                StoredBatchMetadata::delete(wb, index);
            }
        }));

        metadata
    }
}
