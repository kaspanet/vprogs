use std::sync::Arc;

use arc_swap::ArcSwap;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::{BatchMetadata, Checkpoint};
use vprogs_state_metadata::StateMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

/// Shared scheduler state accessible by all components (runtime batches, pruning worker, rollback).
///
/// Unifies the store handle and three key checkpoints into a single `Arc`-wrapped handle:
/// - **root** — oldest surviving batch (advanced by pruning, reset on rollback-to-genesis).
/// - **last_committed** — most recently committed batch (advanced eagerly by `commit_done`, reset
///   on rollback).
/// - **last_processed** — most recently scheduled batch (advanced by `next_checkpoint`, reset on
///   rollback). Only mutated from `&mut Scheduler` context.
///
/// Uses `#[smart_pointer]` so cloning is just `Arc::clone`.
#[smart_pointer]
pub struct SchedulerContext<S: Store<StateSpace = StateSpace>, M: BatchMetadata> {
    pub(crate) store: Arc<S>,
    pub(crate) root: ArcSwap<Checkpoint<M>>,
    pub(crate) last_committed: ArcSwap<Checkpoint<M>>,
    pub(crate) last_processed: ArcSwap<Checkpoint<M>>,
}

impl<S: Store<StateSpace = StateSpace>, M: BatchMetadata> SchedulerContext<S, M> {
    /// Creates a new context by reading persisted state from the store.
    ///
    /// `root` and `last_committed` are loaded from `StateMetadata`. `last_processed` is
    /// initialized to `last_committed` since no new batches have been scheduled yet.
    pub fn new(store: Arc<S>) -> Self {
        let root: Checkpoint<M> = StateMetadata::root(&*store);
        let last_committed: Checkpoint<M> = StateMetadata::last_committed(&*store);
        Self(Arc::new(SchedulerContextData {
            store,
            root: ArcSwap::from_pointee(root),
            last_committed: ArcSwap::from_pointee(last_committed.clone()),
            last_processed: ArcSwap::from_pointee(last_committed),
        }))
    }

    /// Returns a reference to the backing store.
    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    /// Returns the root checkpoint (oldest surviving batch).
    pub fn root(&self) -> Arc<Checkpoint<M>> {
        self.root.load_full()
    }

    /// Returns the most recently committed checkpoint.
    pub fn last_committed(&self) -> Arc<Checkpoint<M>> {
        self.last_committed.load_full()
    }

    /// Returns the most recently processed (scheduled) checkpoint.
    pub fn last_processed(&self) -> Arc<Checkpoint<M>> {
        self.last_processed.load_full()
    }
}
