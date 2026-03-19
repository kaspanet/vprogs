use std::sync::Arc;

use arc_swap::ArcSwapOption;
use vprogs_core_macros::smart_pointer;
use vprogs_core_smt::{Commitment, EMPTY_HASH};
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
pub struct StateDiff<S: Store, P: Processor> {
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

impl<S: Store, P: Processor> StateDiff<S, P> {
    /// Returns the resource ID this state diff tracks.
    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    /// Returns the resource state as it was before this batch executed.
    ///
    /// # Panics
    /// Panics if called before the read state has been resolved.
    pub fn read_state(&self) -> Arc<StateVersion> {
        self.read_state.load_full().expect("read state unknown")
    }

    /// Returns the resource state after this batch executed.
    ///
    /// # Panics
    /// Panics if called before the written state has been resolved.
    pub fn written_state(&self) -> Arc<StateVersion> {
        self.written_state.load_full().expect("written state unknown")
    }

    /// Returns the per-batch resource index.
    pub fn index(&self) -> u32 {
        self.index
    }

    pub(crate) fn new(batch: ScheduledBatchRef<S, P>, resource_id: ResourceId, index: u32) -> Self {
        Self(Arc::new(StateDiffData {
            batch,
            resource_id,
            index,
            read_state: ArcSwapOption::empty(),
            written_state: ArcSwapOption::empty(),
        }))
    }

    /// Returns true if the batch this state diff belongs to has been committed.
    ///
    /// If the batch reference can no longer be upgraded (batch dropped), returns true as the batch
    /// has completed its lifecycle.
    pub(crate) fn was_committed(&self) -> bool {
        self.batch.upgrade().is_none_or(|batch| batch.was_committed())
    }

    pub(crate) fn set_read_state(&self, state: Arc<StateVersion>) {
        self.read_state.store(Some(state))
    }

    pub(crate) fn set_written_state(&self, state: Arc<StateVersion>) {
        self.written_state.store(Some(state));
        if let Some(batch) = self.batch.upgrade() {
            batch.submit_write(Write::StateDiff(self.clone()));
        }
    }

    pub(crate) fn write<W: WriteBatch>(&self, wb: &mut W) {
        let Some(batch) = self.batch.upgrade() else {
            panic!("batch must be known at write time");
        };
        let Some(read_state) = &*self.read_state.load() else {
            panic!("read_state must be known at write time");
        };
        let Some(written_state) = &*self.written_state.load() else {
            panic!("written_state must be known at write time");
        };

        if !batch.was_canceled() {
            written_state.write_data(wb);
            read_state.write_rollback_ptr(wb, batch.checkpoint().index());
        }
    }

    pub(crate) fn write_done(self) {
        if let Some(batch) = self.batch.upgrade() {
            batch.decrease_pending_writes();
        }
    }
}

impl<S: Store, P: Processor> From<&StateDiff<S, P>> for Commitment {
    fn from(diff: &StateDiff<S, P>) -> Self {
        let key = *diff.resource_id.as_bytes();
        let written_state = diff.written_state();
        let data = written_state.data();
        let value_hash = if data.is_empty() { EMPTY_HASH } else { *blake3::hash(data).as_bytes() };
        Self::new(key, value_hash)
    }
}
