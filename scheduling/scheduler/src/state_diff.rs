use std::sync::Arc;

use arc_swap::ArcSwapOption;
use vprogs_core_hashing::Hasher;
use vprogs_core_macros::smart_pointer;
use vprogs_core_smt::{Commitment, EMPTY_HASH, Tree};
use vprogs_core_types::ResourceId;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::{Store, WriteBatch};

use crate::{ScheduledBatchRef, Write, processor::Processor};

/// Tracks the state change for a single resource within a batch.
///
/// Each unique resource accessed by a batch gets one `StateDiff`. It records the read state (before
/// execution) and the written state (after execution), which are used to persist versioned data and
/// rollback pointers.
#[smart_pointer]
pub struct StateDiff<S: Store, P: Processor<S>> {
    /// Weak reference to the owning batch (used for commit checks and write submission).
    batch: ScheduledBatchRef<S, P>,
    /// The resource this diff tracks.
    resource_id: ResourceId,
    /// Position of this resource among all unique resources accessed in the batch (0-based).
    index: u32,
    /// Resource state before the batch executed (set when the first access resolves).
    read_state: ArcSwapOption<StateVersion>,
    /// Resource state after the batch executed (set when the last access commits).
    written_state: ArcSwapOption<StateVersion>,
}

impl<S: Store, P: Processor<S>> StateDiff<S, P> {
    /// Returns the resource ID this state diff tracks.
    #[inline(always)]
    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    /// Returns the resource state before this batch executed; panics if not yet resolved.
    #[inline(always)]
    pub fn read_state(&self) -> Arc<StateVersion> {
        self.read_state.load_full().expect("read state unknown")
    }

    /// Returns the resource state after this batch executed; panics if not yet resolved.
    #[inline(always)]
    pub fn written_state(&self) -> Arc<StateVersion> {
        self.written_state.load_full().expect("written state unknown")
    }

    /// Returns the per-batch resource index.
    #[inline(always)]
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Returns `true` when the batch's execution advanced this resource's version.
    #[inline(always)]
    pub fn data_updated(&self) -> bool {
        self.written_state().version() > self.read_state().version()
    }

    /// Creates a state diff for a resource at its index within the batch.
    pub(crate) fn new(batch: ScheduledBatchRef<S, P>, resource_id: ResourceId, index: u32) -> Self {
        Self(Arc::new(StateDiffData {
            batch,
            resource_id,
            index,
            read_state: ArcSwapOption::empty(),
            written_state: ArcSwapOption::empty(),
        }))
    }

    /// Records the resource state read before execution.
    #[inline(always)]
    pub(crate) fn set_read_state(&self, state: Arc<StateVersion>) {
        self.read_state.store(Some(state))
    }

    /// Records the post-execution state and submits the diff to be persisted.
    pub(crate) fn set_written_state(&self, state: Arc<StateVersion>) {
        self.written_state.store(Some(state));
        if let Some(batch) = self.batch.upgrade().filter(|batch| !batch.restored()) {
            batch.submit_write(Write::StateDiff(self.clone()));
        }
    }

    /// Persists the new version and rollback pointer if the resource's version advanced.
    pub(crate) fn write<W: WriteBatch>(&self, wb: &mut W) {
        // The batch and both states must be resolved by the time the worker writes this diff.
        let Some(batch) = self.batch.upgrade() else {
            panic!("batch must be known at write time");
        };
        let Some(read_state) = &*self.read_state.load() else {
            panic!("read_state must be known at write time");
        };
        let Some(written_state) = &*self.written_state.load() else {
            panic!("written_state must be known at write time");
        };

        // Persist the new version and rollback pointer unless canceled or unchanged.
        if !batch.canceled() && written_state.version() > read_state.version() {
            written_state.write_data(wb);
            read_state.write_rollback_ptr(wb, batch.checkpoint().index());
        }
    }

    /// Returns true if the diff's batch has committed, or was dropped (lifecycle complete).
    #[inline(always)]
    pub(crate) fn committed(&self) -> bool {
        self.batch.upgrade().is_none_or(|batch| batch.committed())
    }
}

impl<S: Store, P: Processor<S>> From<&StateDiff<S, P>> for Commitment {
    /// Hashes the diff's written data into the resource's SMT commitment.
    fn from(diff: &StateDiff<S, P>) -> Self {
        let written_state = diff.written_state();

        Self::new(
            diff.resource_id,
            match written_state.data().as_slice() {
                [] => EMPTY_HASH,
                data => <S as Tree>::Hasher::hash(data),
            },
        )
    }
}
