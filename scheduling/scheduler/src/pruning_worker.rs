use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use tap::Tap;
use tokio::{runtime::Builder, sync::Notify};
use vprogs_core_types::Checkpoint;
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::Store;

use crate::{VmInterface, context::SchedulerContext};

/// Background worker that deletes old state data (rollback pointers and versions) for batches
/// that will never be rolled back, advancing the **root** (oldest surviving batch) forward.
///
/// Runs on a dedicated thread with direct store access to avoid contention with the main
/// write path.
pub struct PruningWorker<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// The batch index up to which pruning is allowed (exclusive).
    /// Batches with index < threshold can be pruned.
    pruning_threshold: Arc<AtomicU64>,
    /// Upper bound on effective pruning threshold. `u64::MAX` means unconstrained.
    /// Set before a rollback to prevent the pruning worker from pruning into the
    /// rollback range; cleared after the rollback completes.
    pause_ceiling: Arc<AtomicU64>,
    /// Tracks the upper bound of the last completed (or in-flight) prune pass.
    /// Monotonically increasing. Used together with `pause_ceiling` in a
    /// Dekker-style handshake (both use `SeqCst`) so that `pause` can detect
    /// an in-flight or already-completed prune that overlaps the rollback range.
    pruning_cursor: Arc<AtomicU64>,
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
        if self.pruning_threshold.fetch_max(threshold, Ordering::AcqRel) < threshold {
            self.notify.notify_one();
        }
    }

    /// Returns the current pruning threshold.
    pub fn threshold(&self) -> u64 {
        self.pruning_threshold.load(Ordering::Acquire)
    }

    /// Pauses pruning at or above the given index.
    ///
    /// Called before a rollback to ensure the pruning worker does not delete state
    /// that the rollback needs. The ceiling clamps the worker's upper bound as
    /// `upper_bound = threshold.min(ceiling) - 1`.
    ///
    /// Returns `true` if the pruning cursor is at or below the ceiling (no
    /// completed or in-flight prune has deleted data the rollback needs),
    /// `false` otherwise. On failure the ceiling is reset to `u64::MAX` so
    /// pruning is not permanently throttled. Both sides use `SeqCst`
    /// (Dekker-style) so at least one side detects the conflict: either the
    /// worker aborts its pass, or this method returns `false`.
    pub fn pause(&self, ceiling: u64) -> bool {
        self.pause_ceiling.store(ceiling, Ordering::SeqCst);
        if self.pruning_cursor.load(Ordering::SeqCst) <= ceiling {
            true
        } else {
            self.pause_ceiling.store(u64::MAX, Ordering::Release);
            false
        }
    }

    /// Unpauses pruning (restores the ceiling to `u64::MAX`) and wakes the worker.
    ///
    /// Called after a rollback completes so pruning can resume normally.
    pub fn unpause(&self) {
        self.pause_ceiling.store(u64::MAX, Ordering::Release);
        self.notify.notify_one();
    }

    /// Creates a new pruning worker with direct store access via the shared context.
    ///
    /// The `root` checkpoint (oldest surviving batch) is managed through the context and shared
    /// for atomic updates. The worker resumes from the persisted root. On a fresh database
    /// (root index 0), no pruning occurs until the first batch commits and initializes root.
    pub(crate) fn new(context: SchedulerContext<S, V::BatchMetadata>) -> Self {
        let root_index = context.root().index();
        let pruning_threshold = Arc::new(AtomicU64::new(root_index));
        let pause_ceiling = Arc::new(AtomicU64::new(u64::MAX));
        let pruning_cursor = Arc::new(AtomicU64::new(root_index.saturating_sub(1)));
        let notify = Arc::new(Notify::new());

        let handle = Self::start(
            context.clone(),
            pruning_threshold.clone(),
            pause_ceiling.clone(),
            pruning_cursor.clone(),
            notify.clone(),
        );

        Self {
            pruning_threshold,
            pause_ceiling,
            pruning_cursor,
            notify,
            handle,
            _marker: PhantomData,
        }
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
        context: SchedulerContext<S, V::BatchMetadata>,
        pruning_threshold: Arc<AtomicU64>,
        pause_ceiling: Arc<AtomicU64>,
        pruning_cursor: Arc<AtomicU64>,
        notify: Arc<Notify>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            Builder::new_current_thread().build().expect("failed to build tokio runtime").block_on(
                async move {
                    // Use strong_count to detect shutdown (when the outer struct is dropped).
                    while Arc::strong_count(&pruning_threshold) != 1 {
                        // Root is the oldest surviving batch — the first candidate for pruning.
                        let lower_bound = context.root().index();

                        // Last batch eligible for pruning: (min of requested threshold and pause
                        // ceiling) converted from exclusive to inclusive. `saturating_sub` guards
                        // against underflow when the effective threshold is 0 (e.g. fresh start
                        // before any `set_threshold` call).
                        let upper_bound = pruning_threshold
                            .load(Ordering::Acquire)
                            .min(pause_ceiling.load(Ordering::Acquire))
                            .saturating_sub(1);

                        // Check if there's pruning work to do.
                        if upper_bound >= lower_bound && lower_bound > 0 {
                            // Advance cursor to signal our prune range (Dekker step 1).
                            let prev = pruning_cursor.swap(upper_bound, Ordering::SeqCst);

                            // Re-check ceiling (Dekker step 2: read their flag). A ceiling may have
                            // been set between our initial read and the store above. If so, restore
                            // the cursor and abort this pass.
                            if pause_ceiling.load(Ordering::SeqCst) < upper_bound {
                                pruning_cursor.store(prev, Ordering::Release);
                                continue;
                            }

                            // Execute pruning directly on the store.
                            // The cursor stays at upper_bound — it now reflects completed progress.
                            Self::prune(&context, lower_bound, upper_bound);
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
    /// Runs on the dedicated pruning thread. Root is advanced in memory first so observers
    /// never see root pointing to already-pruned data. The disk write is committed atomically
    /// with the deletions for crash-fault tolerance.
    fn prune(context: &SchedulerContext<S, V::BatchMetadata>, lower_bound: u64, upper_bound: u64) {
        // New root is the first surviving batch after the pruned range.
        let new_root = Arc::new(Checkpoint::new(
            upper_bound + 1,
            StoredBatchMetadata::get(&*context.store, upper_bound + 1),
        ));

        // Advance root in memory before deleting so no observer sees root pointing to pruned data.
        context.root.store(new_root.clone());

        // Commit all deletions and root update atomically.
        context.store.commit(context.store.write_batch().tap_mut(|wb| {
            // Walk batches from oldest to newest (order doesn't matter for pruning).
            for index in lower_bound..=upper_bound {
                // Delete all rollback pointers and their referenced old versions for this batch.
                for (resource_id, old_version) in
                    StatePtrRollback::iter_batch(&*context.store, index)
                {
                    let resource_id: V::ResourceId =
                        borsh::from_slice(&resource_id).expect("corrupted store: unrecoverable");

                    // Delete the old version data if it exists (version 0 means resource
                    // didn't exist).
                    if old_version != 0 {
                        StateVersion::delete(wb, old_version, &resource_id);
                    }

                    // Delete the rollback pointer itself.
                    StatePtrRollback::delete(wb, index, &resource_id);
                }

                // Delete batch metadata entries for this batch.
                StoredBatchMetadata::delete(wb, index);
            }

            // Advance root on disk.
            StateMetadata::set_root(wb, &new_root);
        }));
    }
}
