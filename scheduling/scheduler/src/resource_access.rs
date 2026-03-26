use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use arc_swap::ArcSwapOption;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::{AccessMetadata, AccessType};
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageManager;
use vprogs_storage_types::{ReadStore, Store};

use crate::{Read, ScheduledTransactionRef, StateDiff, Write, processor::Processor};

/// A single resource access within a transaction, forming a linked chain across the batch.
///
/// Each `ResourceAccess` represents one transaction's claim on a resource. Within a batch, accesses
/// to the same resource are linked via `prev`/`next` pointers so that write results flow forward to
/// dependent reads. Derefs to the inner `AccessMetadata`.
#[smart_pointer(deref(access_metadata))]
pub struct ResourceAccess<S: Store, P: Processor<S>> {
    /// The access metadata (deref target).
    access_metadata: AccessMetadata,
    /// True if this is the first access to the resource in this batch.
    is_batch_head: AtomicBool,
    /// True if this is the last access to the resource in this batch.
    is_batch_tail: AtomicBool,
    /// Weak reference to the owning transaction.
    tx: ScheduledTransactionRef<S, P>,
    /// Shared state diff for this resource within the batch.
    state_diff: StateDiff<S, P>,
    /// Resource state before this access (resolved from disk or the previous access).
    read_state: ArcSwapOption<StateVersion>,
    /// Resource state after this access (set on commit or forwarded from read for reads).
    written_state: ArcSwapOption<StateVersion>,
    /// Previous access to the same resource in this batch (cleared once read state resolves).
    prev: ArcSwapOption<Self>,
    /// Next access to the same resource in this batch (cleared once written state propagates).
    next: ArcSwapOption<Self>,
}

impl<S: Store, P: Processor<S>> ResourceAccess<S, P> {
    /// Returns the resource state as it was before this access.
    #[inline(always)]
    pub fn read_state(&self) -> Arc<StateVersion> {
        self.read_state.load_full().expect("read state unknown")
    }

    /// Returns the resource state after this access completed.
    #[inline(always)]
    pub fn written_state(&self) -> Arc<StateVersion> {
        self.written_state.load_full().expect("written state unknown")
    }

    /// Returns true if this is the first access to the resource within the batch.
    #[inline(always)]
    pub fn is_batch_head(&self) -> bool {
        self.is_batch_head.load(Ordering::Relaxed)
    }

    /// Returns true if this is the last access to the resource within the batch.
    #[inline(always)]
    pub fn is_batch_tail(&self) -> bool {
        self.is_batch_tail.load(Ordering::Relaxed)
    }

    /// Returns the per-batch resource index.
    #[inline(always)]
    pub fn resource_index(&self) -> u32 {
        self.state_diff.index()
    }

    pub(crate) fn new(
        access_metadata: AccessMetadata,
        tx: ScheduledTransactionRef<S, P>,
        state_diff: StateDiff<S, P>,
        prev: Option<Self>,
    ) -> Self {
        Self(Arc::new(ResourceAccessData {
            access_metadata,
            is_batch_head: AtomicBool::new(match &prev {
                Some(prev) if prev.state_diff == state_diff => {
                    prev.is_batch_tail.store(false, Ordering::Relaxed);
                    false
                }
                _ => true,
            }),
            is_batch_tail: AtomicBool::new(true),
            tx,
            state_diff,
            read_state: ArcSwapOption::empty(),
            written_state: ArcSwapOption::empty(),
            prev: ArcSwapOption::new(prev.map(|p| p.0)),
            next: ArcSwapOption::empty(),
        }))
    }

    pub(crate) fn connect(&self, storage: &StorageManager<S, Read<S, P>, Write<S, P>>) {
        match &*self.prev.load() {
            Some(prev) => {
                prev.next.store(Some(self.0.clone()));
                if let Some(written_state) = prev.written_state.load_full() {
                    self.set_read_state(written_state);
                }
            }
            None => storage.submit_read(Read::LatestData(self.clone())),
        }
    }

    pub(crate) fn read_latest_data<R: ReadStore>(&self, store: &R) {
        self.set_read_state(Arc::new(StateVersion::from_latest_data(
            store,
            self.access_metadata.resource_id,
        )));
    }

    pub(crate) fn tx(&self) -> &ScheduledTransactionRef<S, P> {
        &self.tx
    }

    pub(crate) fn state_diff(&self) -> StateDiff<S, P> {
        self.state_diff.clone()
    }

    /// Returns true if the state diff this resource access belongs to has been committed.
    pub(crate) fn committed(&self) -> bool {
        self.state_diff.committed()
    }

    pub(crate) fn set_read_state(&self, state: Arc<StateVersion>) {
        if self.read_state.compare_and_swap(&None::<Arc<_>>, Some(state.clone())).is_none() {
            drop(self.prev.swap(None)); // drop the previous reference to allow cleanup

            if self.is_batch_head() {
                self.state_diff.set_read_state(state.clone());
            }

            if self.access_type == AccessType::Read {
                self.set_written_state(state);
            }

            if let Some(tx) = self.tx.upgrade() {
                tx.decrease_pending_resources();
            }
        }
    }

    pub(crate) fn set_written_state(&self, state: Arc<StateVersion>) {
        if self.written_state.compare_and_swap(&None::<Arc<_>>, Some(state.clone())).is_none() {
            if self.is_batch_tail() {
                self.state_diff.set_written_state(state.clone());
            }

            if let Some(next) = self.next.swap(None) {
                Self(next).set_read_state(state)
            }
        }
    }
}
