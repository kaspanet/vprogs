use std::sync::Arc;

use arc_swap::ArcSwapOption;
use vprogs_core_macros::smart_pointer;
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::{Store, WriteBatch};

use crate::{RuntimeBatchRef, Write, transaction_processor::TransactionProcessor};

/// Tracks the state change for a single resource within a batch.
///
/// Each unique resource accessed by a batch gets one `StateDiff`. It records the read state (before
/// execution) and the written state (after execution), which are used to persist versioned data and
/// rollback pointers.
#[smart_pointer]
pub struct StateDiff<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> {
    /// Weak reference to the owning batch (used for commit checks and write submission).
    batch: RuntimeBatchRef<S, V>,
    /// The resource this diff tracks.
    resource_id: V::ResourceId,
    /// Resource state before the batch executed (set when the first access resolves).
    read_state: ArcSwapOption<StateVersion<V::ResourceId>>,
    /// Resource state after the batch executed (set when the last access commits).
    written_state: ArcSwapOption<StateVersion<V::ResourceId>>,
}

impl<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> StateDiff<S, V> {
    /// Returns the resource ID this state diff tracks.
    pub fn resource_id(&self) -> &V::ResourceId {
        &self.resource_id
    }

    /// Returns the resource state as it was before this batch executed.
    ///
    /// # Panics
    /// Panics if called before the read state has been resolved.
    pub fn read_state(&self) -> Arc<StateVersion<V::ResourceId>> {
        self.read_state.load_full().expect("read state unknown")
    }

    /// Returns the resource state after this batch executed.
    ///
    /// # Panics
    /// Panics if called before the written state has been resolved.
    pub fn written_state(&self) -> Arc<StateVersion<V::ResourceId>> {
        self.written_state.load_full().expect("written state unknown")
    }

    pub(crate) fn new(batch: RuntimeBatchRef<S, V>, resource_id: V::ResourceId) -> Self {
        Self(Arc::new(StateDiffData {
            batch,
            resource_id,
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

    pub(crate) fn set_read_state(&self, state: Arc<StateVersion<V::ResourceId>>) {
        self.read_state.store(Some(state))
    }

    pub(crate) fn set_written_state(&self, state: Arc<StateVersion<V::ResourceId>>) {
        self.written_state.store(Some(state));
        if let Some(batch) = self.batch.upgrade() {
            batch.submit_write(Write::StateDiff(self.clone()));
        }
    }

    pub(crate) fn write<W: WriteBatch<StateSpace = StateSpace>>(&self, wb: &mut W) {
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
